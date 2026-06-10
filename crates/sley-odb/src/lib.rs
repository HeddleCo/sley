use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::{Bundle, BundleReference};
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries, parse_framed_object};
use sley_pack::{MultiPackIndex, PackFile, PackIndex, PackIndexEntry, PackInput, PackWrite};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::{env, fs};

static TEMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub trait ObjectReader {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>>;
}

pub trait ObjectWriter {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleUnbundleResult {
    pub written_objects: Vec<ObjectId>,
    pub references: Vec<BundleReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackUnpackResult {
    pub written_objects: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackInstallResult {
    pub pack_name: String,
    pub pack_path: PathBuf,
    pub index_path: PathBuf,
    pub promisor_path: Option<PathBuf>,
    pub object_ids: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPackInstallResult {
    pub object_ids: Vec<ObjectId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawPackInstallOptions {
    pub promisor: bool,
}

pub trait RawPackInstaller {
    fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectPrefixResolution {
    Missing,
    Unique(ObjectId),
    Ambiguous(Vec<ObjectId>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStorageInfo {
    pub disk_size: u64,
    pub deltabase: ObjectId,
}

impl RawPackInstaller for FileObjectDatabase {
    fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult> {
        let result = FileObjectDatabase::install_raw_pack(self, pack_bytes)?;
        Ok(RawPackInstallResult {
            object_ids: result.object_ids,
        })
    }
}

impl RawPackInstaller for std::cell::RefCell<ObjectDatabase> {
    fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult> {
        let mut database = self.borrow_mut();
        let format = database.format;
        let result = unpack_packfile_objects(pack_bytes, format, &mut *database)?;
        Ok(RawPackInstallResult {
            object_ids: result.written_objects,
        })
    }
}

pub fn verify_bundle_prerequisites<R: ObjectReader>(bundle: &Bundle, reader: &R) -> Result<()> {
    let mut missing = Vec::new();
    for prerequisite in &bundle.prerequisites {
        match reader.read_object(&prerequisite.oid) {
            Ok(object) => {
                let actual = object.object_id(bundle.format)?;
                if actual != prerequisite.oid {
                    return Err(GitError::InvalidObject(format!(
                        "bundle prerequisite {} hashes to {actual}",
                        prerequisite.oid
                    )));
                }
            }
            Err(GitError::NotFound(_)) => missing.push(prerequisite.oid),
            Err(err) => return Err(err),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let missing = missing
        .iter()
        .map(ObjectId::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Err(GitError::not_found(format!(
        "bundle prerequisites missing: {missing}"
    )))
}

pub fn unbundle_objects<R, W>(
    bundle: &Bundle,
    prerequisite_reader: &R,
    writer: &mut W,
) -> Result<BundleUnbundleResult>
where
    R: ObjectReader,
    W: ObjectWriter,
{
    verify_bundle_prerequisites(bundle, prerequisite_reader)?;
    let pack = PackFile::parse_bundle(bundle)?;
    let written_objects = write_pack_objects(pack, writer, "bundle")?.written_objects;
    Ok(BundleUnbundleResult {
        written_objects,
        references: bundle.references.clone(),
    })
}

pub fn install_bundle_pack<R>(
    bundle: &Bundle,
    prerequisite_reader: &R,
    destination: &impl RawPackInstaller,
) -> Result<BundleUnbundleResult>
where
    R: ObjectReader,
{
    verify_bundle_prerequisites(bundle, prerequisite_reader)?;
    let install = destination.install_raw_pack(&bundle.pack)?;
    Ok(BundleUnbundleResult {
        written_objects: install.object_ids,
        references: bundle.references.clone(),
    })
}

pub fn unpack_packfile_objects<W>(
    pack_bytes: &[u8],
    format: ObjectFormat,
    writer: &mut W,
) -> Result<PackUnpackResult>
where
    W: ObjectWriter,
{
    let pack = PackFile::parse(pack_bytes, format)?;
    write_pack_objects(pack, writer, "pack")
}

fn write_pack_objects<W>(pack: PackFile, writer: &mut W, source: &str) -> Result<PackUnpackResult>
where
    W: ObjectWriter,
{
    let mut written_objects = Vec::with_capacity(pack.entries.len());
    for entry in pack.entries {
        let expected = entry.entry.oid;
        let actual = writer.write_object(entry.object)?;
        if actual != expected {
            return Err(GitError::InvalidObject(format!(
                "{source} object id mismatch: expected {expected}, wrote {actual}"
            )));
        }
        written_objects.push(actual);
    }
    Ok(PackUnpackResult { written_objects })
}

pub fn collect_reachable_object_ids<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    walk_reachable_objects(reader, format, starts, &HashSet::new(), |_, _| {})
}

pub fn collect_reachable_objects<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<Vec<Arc<EncodedObject>>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let mut objects = Vec::new();
    walk_reachable_objects(reader, format, starts, excluded, |_, object| {
        objects.push(Arc::clone(object));
    })?;
    Ok(objects)
}

#[derive(Debug, Clone)]
struct ReachablePackObject {
    oid: ObjectId,
    object: Arc<EncodedObject>,
}

fn collect_reachable_pack_objects<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<Vec<ReachablePackObject>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let mut objects = Vec::new();
    walk_reachable_objects(reader, format, starts, excluded, |oid, object| {
        objects.push(ReachablePackObject {
            oid: *oid,
            object: Arc::clone(object),
        });
    })?;
    Ok(objects)
}

fn pack_inputs(objects: &[ReachablePackObject]) -> Vec<PackInput<'_>> {
    objects
        .iter()
        .map(|entry| PackInput {
            oid: &entry.oid,
            object: &entry.object,
        })
        .collect()
}

pub fn install_reachable_pack<I>(
    source: &impl ObjectReader,
    destination: &impl RawPackInstaller,
    format: ObjectFormat,
    starts: I,
) -> Result<Option<RawPackInstallResult>>
where
    I: IntoIterator<Item = ObjectId>,
{
    install_reachable_pack_excluding(source, destination, format, starts, &HashSet::new())
}

pub fn install_reachable_pack_excluding<I>(
    source: &impl ObjectReader,
    destination: &impl RawPackInstaller,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<Option<RawPackInstallResult>>
where
    I: IntoIterator<Item = ObjectId>,
{
    let pack = match build_reachable_pack(source, format, starts, excluded)? {
        Some(pack) => pack,
        None => return Ok(None),
    };
    destination.install_raw_pack(&pack.pack).map(Some)
}

pub fn build_reachable_pack<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<Option<PackWrite>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let objects = collect_reachable_pack_objects(reader, format, starts, excluded)?;
    if objects.is_empty() {
        return Ok(None);
    }
    // Delta-compress reachable packs (used by install/push/fetch) via git-pack's
    // sliding-window selection. Self-contained, ofs-delta by default; round-trips
    // through the existing parser. PackWrite shape is unchanged, so callers are
    // unaffected.
    let inputs = pack_inputs(&objects);
    PackFile::write_packed_with_known_ids(&inputs, format).map(Some)
}

pub fn build_and_install_reachable_pack<R, I>(
    source: &R,
    destination: &FileObjectDatabase,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
    options: RawPackInstallOptions,
) -> Result<Option<PackInstallResult>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let Some(pack) = build_reachable_pack(source, format, starts, excluded)? else {
        return Ok(None);
    };
    destination
        .install_generated_pack_unchecked(&pack, options)
        .map(Some)
}

/// Outcome of consolidating every object in a repository into a single pack.
///
/// This is the engine for `git gc` / `git repack`: [`repack_all_objects`]
/// produces the bytes for one new delta-compressed pack plus its index, and
/// reports which on-disk artifacts the caller could now remove. No deletions
/// are performed by the engine itself; the CLI decides reachability policy and
/// performs any pruning (see [`install_repack_result`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackResult {
    /// Bytes of the freshly written `.pack` file.
    pub pack: Vec<u8>,
    /// Bytes of the matching `.idx` file for [`RepackResult::pack`].
    pub idx: Vec<u8>,
    /// Number of distinct objects contained in the new pack.
    pub object_count: usize,
    /// Absolute paths of pre-existing `*.pack` files now superseded by the new
    /// pack (every object they hold is present in [`RepackResult::pack`]).
    pub obsolete_packs: Vec<PathBuf>,
    /// Loose object ids that are now also present in the new pack and therefore
    /// redundant on disk.
    pub packed_loose: Vec<ObjectId>,
    pack_checksum: ObjectId,
    index_entries: Vec<PackIndexEntry>,
}

/// Gather every object in `git_dir` (loose objects and every existing pack) and
/// write them into a single new delta-compressed pack.
///
/// Returns the new pack/index bytes, the count of packed objects, the list of
/// pre-existing pack files that the new pack supersedes, and the loose object
/// ids that are now packed. Nothing is deleted: the caller (CLI) decides
/// reachability policy and performs any pruning, optionally via
/// [`install_repack_result`].
///
/// Returns `Ok(None)` when the repository contains no objects at all.
pub fn repack_all_objects(git_dir: &Path, format: ObjectFormat) -> Result<Option<RepackResult>> {
    let objects_dir = repository_objects_dir(git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);

    // Enumerate every object id reachable on disk: loose objects, every pack
    // index, and any multi-pack-index. `object_ids_in_objects_dir` already
    // unions all of these and de-duplicates them.
    let all_oids = object_ids_in_objects_dir(&objects_dir, format)?;
    if all_oids.is_empty() {
        return Ok(None);
    }

    // Read each object's canonical encoding so the new pack stores byte-for-byte
    // identical payloads. Loose objects take precedence over packed copies in
    // `FileObjectDatabase::read_object`, but both decode to the same bytes.
    let mut objects = Vec::with_capacity(all_oids.len());
    for oid in &all_oids {
        objects.push(ReachablePackObject {
            oid: *oid,
            object: database.read_object(oid)?,
        });
    }

    let inputs = pack_inputs(&objects);
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let object_count = written.entries.len();

    // The new pack contains every object on disk, so every pre-existing pack is
    // fully superseded. We still record the exact pack paths (not the index
    // paths) so the caller can delete the right files. The pack we are about to
    // write is excluded by name in case its checksum collides with an existing
    // pack (identical contents).
    let new_pack_file_name = format!("pack-{}.pack", written.checksum.to_hex());
    let obsolete_packs = existing_pack_files(&objects_dir.join("pack"))?
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) != Some(&new_pack_file_name))
        .collect();

    // Loose object ids that the new pack now also holds (which is all of them,
    // since they were gathered into it).
    let packed_oid_set: HashSet<&ObjectId> = written.entries.iter().map(|e| &e.oid).collect();
    let mut packed_loose: Vec<ObjectId> = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .filter(|oid| packed_oid_set.contains(oid))
        .collect();
    packed_loose.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    Ok(Some(RepackResult {
        pack: written.pack,
        idx: written.index,
        object_count,
        obsolete_packs,
        packed_loose,
        pack_checksum: written.checksum,
        index_entries: written.entries,
    }))
}

/// Write the consolidated pack from a [`RepackResult`] into
/// `objects/pack/` and, when `prune` is set, remove the now-redundant
/// pre-existing packs and packed loose objects.
///
/// Pruning is opt-in and deliberately conservative: an object or pack is only
/// removed after verifying it is actually present in the freshly written pack
/// on disk. Concretely:
///
/// * a loose object is removed only if its id appears in the new pack;
/// * a pre-existing pack is removed only if it is not the pack we just wrote
///   *and* every object listed in its `.idx` is present in the new pack (its
///   `.idx` and known sidecars are removed alongside it);
/// * a stale `multi-pack-index` is removed only if every pack it references is
///   being removed, so no reader is ever left pointing at a deleted pack.
pub fn install_repack_result(
    git_dir: &Path,
    format: ObjectFormat,
    result: &RepackResult,
    prune: bool,
) -> Result<()> {
    let objects_dir = repository_objects_dir(git_dir);
    let pack_dir = objects_dir.join("pack");
    fs::create_dir_all(&pack_dir)?;

    // Validate the public bytes against the private provenance that
    // `repack_all_objects` captured from `PackFile::write_packed`. This avoids
    // inflating and resolving the freshly-written pack a second time while still
    // catching caller mutations before anything is written or pruned.
    validate_pack_checksum(&result.pack, format, &result.pack_checksum, "repack")?;
    let parsed_index = PackIndex::parse(&result.idx, format)?;
    if parsed_index.pack_checksum != result.pack_checksum {
        return Err(GitError::InvalidFormat(
            "repack index checksum does not match the new pack".into(),
        ));
    }
    if !pack_index_entries_match_writer(&parsed_index.entries, &result.index_entries) {
        return Err(GitError::InvalidFormat(
            "repack index does not match the new pack contents".into(),
        ));
    }
    let pack_name = format!("pack-{}", result.pack_checksum.to_hex());
    let new_pack_path = pack_dir.join(format!("{pack_name}.pack"));
    let new_index_path = pack_dir.join(format!("{pack_name}.idx"));
    write_pack_component(&new_pack_path, &result.pack)?;
    write_pack_component(&new_index_path, &result.idx)?;

    if !prune {
        return Ok(());
    }

    // Prune based on the objects the new pack's *index* can resolve (what reads use
    // once the old packs are gone), not just what the pack contains — so a stale
    // pack is never removed for an object the new index cannot serve.
    let present: HashSet<ObjectId> = parsed_index.entries.iter().map(|entry| entry.oid).collect();

    prune_packs_contained_in(&objects_dir, format, &present, &new_pack_path)?;
    prune_loose_objects(&objects_dir, format, result.packed_loose.iter(), &present)?;
    Ok(())
}

fn validate_pack_checksum(
    pack: &[u8],
    format: ObjectFormat,
    expected: &ObjectId,
    context: &str,
) -> Result<()> {
    if expected.format() != format {
        return Err(GitError::InvalidObjectId(format!(
            "{context} checksum format does not match object format"
        )));
    }
    let hash_len = format.raw_len();
    if pack.len() < 12 + hash_len {
        return Err(GitError::InvalidFormat(format!(
            "{context} pack file too short"
        )));
    }
    if &pack[..4] != b"PACK" {
        return Err(GitError::InvalidFormat(format!(
            "{context} pack file missing PACK signature"
        )));
    }
    let trailer_offset = pack.len() - hash_len;
    let actual = sley_core::digest_bytes(format, &pack[..trailer_offset])?;
    let trailer = ObjectId::from_raw(format, &pack[trailer_offset..])?;
    if &actual != expected || trailer != *expected {
        return Err(GitError::InvalidFormat(format!(
            "{context} pack checksum does not match generated pack"
        )));
    }
    Ok(())
}

fn pack_index_entries_match_writer(
    parsed: &[PackIndexEntry],
    writer_entries: &[PackIndexEntry],
) -> bool {
    if parsed.len() != writer_entries.len() {
        return false;
    }
    let mut writer_entries = writer_entries.iter().collect::<Vec<_>>();
    writer_entries.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
    parsed.iter().zip(writer_entries).all(|(left, right)| {
        left.oid == right.oid && left.crc32 == right.crc32 && left.offset == right.offset
    })
}

/// List loose objects under `git_dir` that are *not* reachable from `roots`,
/// optionally deleting them.
///
/// Reachability is computed with [`collect_reachable_object_ids`] over the
/// repository's object database, so trees, parents, and tag targets are all
/// followed. When `delete` is `false` the returned ids are merely reported;
/// when `true` each unreachable loose object file is removed (packed copies are
/// never touched). Deletion is therefore opt-in.
pub fn prune_unreachable_loose<I>(
    git_dir: &Path,
    format: ObjectFormat,
    roots: I,
    delete: bool,
) -> Result<Vec<ObjectId>>
where
    I: IntoIterator<Item = ObjectId>,
{
    let objects_dir = repository_objects_dir(git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);
    let reachable = collect_reachable_object_ids(&database, format, roots)?;

    let store = LooseObjectStore::new(objects_dir.clone(), format);
    let mut pruned: Vec<ObjectId> = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .filter(|oid| !reachable.contains(oid))
        .collect();
    pruned.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    if delete {
        for oid in &pruned {
            let path = store.object_path(oid)?;
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
        }
    }
    Ok(pruned)
}

/// Loose object ids under `objects_dir`, sorted by hex, with packed objects
/// excluded.
fn loose_object_ids(objects_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let mut oids = HashSet::new();
    collect_loose_object_ids(objects_dir, format, &mut oids)?;
    let mut oids = oids.into_iter().collect::<Vec<_>>();
    oids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(oids)
}

/// Absolute paths of every `*.pack` file directly inside `pack_dir`, sorted for
/// deterministic output.
fn existing_pack_files(pack_dir: &Path) -> Result<Vec<PathBuf>> {
    if !pack_dir.exists() {
        return Ok(Vec::new());
    }
    let mut packs = Vec::new();
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("pack") && path.is_file() {
            packs.push(path);
        }
    }
    packs.sort();
    Ok(packs)
}

/// Remove pre-existing packs whose every object is contained in `present`,
/// skipping `keep` (the pack just written), `.keep` packs, and `.promisor` packs.
/// A stale multi-pack-index that references any removed pack is removed too.
fn prune_packs_contained_in(
    objects_dir: &Path,
    format: ObjectFormat,
    present: &HashSet<ObjectId>,
    keep: &Path,
) -> Result<()> {
    let pack_dir = objects_dir.join("pack");
    let keep_stem = keep.file_stem().map(|stem| stem.to_owned());
    let mut removed_stems: HashSet<String> = HashSet::new();

    for pack_path in existing_pack_files(&pack_dir)? {
        if pack_path == keep {
            continue;
        }
        let Some(stem) = pack_path.file_stem() else {
            continue;
        };
        if Some(stem) == keep_stem.as_deref() {
            continue;
        }
        if pack_path.with_extension("keep").exists()
            || pack_path.with_extension("promisor").exists()
        {
            continue;
        }
        let index_path = pack_path.with_extension("idx");
        if !index_path.exists() {
            // Without an index we cannot prove containment; leave it alone.
            continue;
        }
        let index = PackIndex::parse(&fs::read(&index_path)?, format)?;
        if !index
            .entries
            .iter()
            .all(|entry| present.contains(&entry.oid))
        {
            continue;
        }
        // Every object in this pack is safely in the new pack and it has no Git
        // policy sidecar that says to keep it: remove the pack, its index, and
        // cache sidecars derived from them.
        remove_file_if_exists(&pack_path)?;
        remove_file_if_exists(&index_path)?;
        for ext in ["rev", "mtimes", "bitmap"] {
            remove_file_if_exists(&pack_path.with_extension(ext))?;
        }
        removed_stems.insert(stem.to_string_lossy().into_owned());
    }

    prune_stale_multi_pack_index(&pack_dir, format, &removed_stems)?;
    Ok(())
}

/// Remove a `multi-pack-index` if it names *any* pack that was removed.
///
/// A MIDX that still references a deleted pack makes reads fail (the lookup
/// resolves to a pack that is gone) before any fallback. Removing the whole MIDX
/// when even one of its packs is pruned forces readers back to the individual pack
/// indexes, which are correct; `multi-pack-index write` can rebuild it later.
fn prune_stale_multi_pack_index(
    pack_dir: &Path,
    format: ObjectFormat,
    removed_stems: &HashSet<String>,
) -> Result<()> {
    if removed_stems.is_empty() {
        return Ok(());
    }
    let midx_path = pack_dir.join("multi-pack-index");
    if !midx_path.exists() {
        return Ok(());
    }
    let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, format)?;
    let references_removed_pack = midx.pack_names.iter().any(|name| {
        let stem = name.strip_suffix(".idx").unwrap_or(name);
        removed_stems.contains(stem)
    });
    if references_removed_pack {
        remove_file_if_exists(&midx_path)?;
    }
    Ok(())
}

/// Remove each loose object in `candidates` whose id is in `present`, leaving
/// any object not actually packed untouched.
fn prune_loose_objects<'a, I>(
    objects_dir: &Path,
    format: ObjectFormat,
    candidates: I,
    present: &HashSet<ObjectId>,
) -> Result<()>
where
    I: IntoIterator<Item = &'a ObjectId>,
{
    let store = LooseObjectStore::new(objects_dir.to_path_buf(), format);
    for oid in candidates {
        if !present.contains(oid) {
            continue;
        }
        remove_file_if_exists(&store.object_path(oid)?)?;
    }
    Ok(())
}

enum PackDeltaBase {
    Offset(u64),
    Ref(ObjectId),
}

struct PackIndexOffsetInfo {
    end_offset: u64,
    delta_base_oid: Option<ObjectId>,
}

fn scan_pack_index_offsets(
    index: &PackIndex,
    target_offset: u64,
    trailer_offset: u64,
    delta_base_offset: Option<u64>,
) -> Result<PackIndexOffsetInfo> {
    let mut target_count = 0usize;
    let mut next_offset = None;
    let mut delta_base_oid = None;

    for entry in &index.entries {
        if entry.offset == target_offset {
            target_count += 1;
        } else if entry.offset > target_offset {
            match next_offset {
                Some(current) if current <= entry.offset => {}
                _ => next_offset = Some(entry.offset),
            }
        }
        if Some(entry.offset) == delta_base_offset {
            delta_base_oid = Some(entry.oid);
        }
    }

    if target_count == 0 {
        return Err(GitError::InvalidFormat(format!(
            "pack index offset {target_offset} not found"
        )));
    }
    if let Some(offset) = delta_base_offset
        && delta_base_oid.is_none()
    {
        return Err(GitError::InvalidFormat(format!(
            "ofs-delta base offset {offset} not found"
        )));
    }

    Ok(PackIndexOffsetInfo {
        // Preserve the old sorted-vector behavior for malformed indexes with
        // duplicate offsets: the next sorted entry has the same offset.
        end_offset: if target_count > 1 {
            target_offset
        } else {
            next_offset.unwrap_or(trailer_offset)
        },
        delta_base_oid,
    })
}

fn pack_entry_delta_base(
    format: ObjectFormat,
    pack: &[u8],
    entry_offset: u64,
) -> Result<Option<PackDeltaBase>> {
    let mut cursor = usize::try_from(entry_offset)
        .map_err(|_| GitError::InvalidFormat("pack entry offset overflows usize".into()))?;
    let first = pack_next_byte(pack, &mut cursor)?;
    let kind = (first >> 4) & 0x07;
    let mut byte = first;
    while byte & 0x80 != 0 {
        byte = pack_next_byte(pack, &mut cursor)?;
    }
    match kind {
        6 => Ok(Some(PackDeltaBase::Offset(parse_ofs_delta_base_offset(
            pack,
            &mut cursor,
            entry_offset,
        )?))),
        7 => Ok(Some(PackDeltaBase::Ref(parse_ref_delta_base_oid(
            format,
            pack,
            &mut cursor,
        )?))),
        _ => Ok(None),
    }
}

fn parse_ref_delta_base_oid(
    format: ObjectFormat,
    pack: &[u8],
    cursor: &mut usize,
) -> Result<ObjectId> {
    let raw_len = format.raw_len();
    if *cursor + raw_len > pack.len() {
        return Err(GitError::InvalidFormat(
            "truncated ref-delta base object id".into(),
        ));
    }
    let oid = ObjectId::from_raw(format, &pack[*cursor..*cursor + raw_len])?;
    *cursor += raw_len;
    Ok(oid)
}

fn parse_ofs_delta_base_offset(pack: &[u8], cursor: &mut usize, entry_offset: u64) -> Result<u64> {
    let mut byte = pack_next_byte(pack, cursor)?;
    let mut relative = u64::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = pack_next_byte(pack, cursor)?;
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

fn pack_next_byte(pack: &[u8], cursor: &mut usize) -> Result<u8> {
    let Some(byte) = pack.get(*cursor).copied() else {
        return Err(GitError::InvalidFormat("truncated pack entry".into()));
    };
    *cursor += 1;
    Ok(byte)
}

fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}

/// Remove `path` if it exists, treating a missing file as success.
fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

fn walk_reachable_objects<R, I, F>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
    mut visit: F,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
    F: FnMut(&ObjectId, &Arc<EncodedObject>),
{
    let mut seen = HashSet::new();
    let mut pending = Vec::new();
    for start in starts {
        pending.push(start);
        while let Some(oid) = pending.pop() {
            if excluded.contains(&oid) {
                continue;
            }
            if !seen.insert(oid) {
                continue;
            }
            let object = reader.read_object(&oid)?;
            match object.object_type {
                ObjectType::Commit => {
                    let (tree, parents) = {
                        let commit = Commit::parse_ref(format, &object.body)?;
                        (commit.tree, commit.parents)
                    };
                    visit(&oid, &object);
                    for parent in parents.into_iter().rev() {
                        pending.push(parent);
                    }
                    pending.push(tree);
                }
                ObjectType::Tree => {
                    let mut child_oids = Vec::new();
                    for entry in TreeEntries::new(format, &object.body) {
                        let entry = entry?;
                        if entry.is_gitlink() {
                            continue;
                        }
                        child_oids.push(entry.oid);
                    }
                    visit(&oid, &object);
                    pending.extend(child_oids.into_iter().rev());
                }
                ObjectType::Tag => {
                    let target = {
                        let tag = Tag::parse_ref(format, &object.body)?;
                        tag.object
                    };
                    visit(&oid, &object);
                    pending.push(target);
                }
                ObjectType::Blob => visit(&oid, &object),
            }
        }
    }
    Ok(seen)
}

#[derive(Debug, Clone)]
pub struct ObjectDatabase {
    format: ObjectFormat,
    objects: HashMap<ObjectId, Arc<EncodedObject>>,
    promisor: bool,
}

impl ObjectDatabase {
    pub fn new(format: ObjectFormat) -> Self {
        Self {
            format,
            objects: HashMap::new(),
            promisor: false,
        }
    }

    pub fn with_promisor(mut self, promisor: bool) -> Self {
        self.promisor = promisor;
        self
    }

    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.objects.contains_key(oid)
    }

    pub fn validate(&self, oid: &ObjectId) -> Result<()> {
        let object = self.read_object(oid)?;
        let actual = object.object_id(self.format)?;
        if &actual == oid {
            Ok(())
        } else {
            Err(GitError::InvalidObject(format!(
                "object id mismatch: expected {oid}, got {actual}"
            )))
        }
    }
}

impl ObjectReader for ObjectDatabase {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        self.objects
            .get(oid)
            .map(Arc::clone)
            .ok_or_else(|| GitError::object_not_found(*oid))
    }
}

impl ObjectWriter for ObjectDatabase {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        self.objects.entry(oid).or_insert_with(|| Arc::new(object));
        Ok(oid)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternate {
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialClonePolicy {
    pub promisor_remote: Option<String>,
    pub allow_missing_promised_objects: bool,
}

/// Raw pack-file bytes keyed by pack path, shared across cloned handles. Loaded
/// once so individual objects can be decoded at their offsets (see
/// [`sley_pack::read_object_at`]) without re-reading the whole file per read.
type PackBytesCache = Arc<Mutex<HashMap<PathBuf, Arc<PackData>>>>;

/// Backing bytes of a pack file: either memory-mapped (under the `mmap` feature)
/// or read into the heap. Both deref to `&[u8]`, so the decode path is identical.
#[derive(Debug)]
enum PackData {
    #[cfg(feature = "mmap")]
    Mapped(sley_mmap::MappedFile),
    Heap(Vec<u8>),
}

impl std::ops::Deref for PackData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped(mapped) => mapped,
            Self::Heap(bytes) => bytes,
        }
    }
}

/// Load a pack file's bytes: memory-mapped when the `mmap` feature is on (falling
/// back to a heap read if the map fails), otherwise read into the heap.
#[cfg(feature = "mmap")]
fn load_pack_data(pack_path: &Path) -> Result<PackData> {
    match sley_mmap::MappedFile::open_pack(pack_path) {
        Ok(mapped) => Ok(PackData::Mapped(mapped)),
        Err(_) => Ok(PackData::Heap(fs::read(pack_path)?)),
    }
}

#[cfg(not(feature = "mmap"))]
fn load_pack_data(pack_path: &Path) -> Result<PackData> {
    Ok(PackData::Heap(fs::read(pack_path)?))
}

/// Memory-capped LRU of recently decoded objects, shared across cloned handles,
/// so hot delta bases and repeated reads during a walk aren't re-decoded. The
/// cache is bounded by an approximate byte budget (not a fixed object count) so
/// it neither thrashes on bulk reads of small objects nor blows up on a few
/// large ones.
type DecodedObjectCache = Arc<Mutex<LruObjectCache>>;

/// Per-pack caches of objects decoded from a pack, keyed by pack path and then by
/// the in-pack byte offset of each object's entry. Shared across cloned handles.
/// This is the delta-base cache: resolving a delta chain by offset reuses already
/// decoded bases instead of re-inflating the whole chain on every read.
type PackDeltaCaches = Arc<Mutex<HashMap<PathBuf, Arc<Mutex<LruOffsetCache>>>>>;

/// Default approximate byte budget for the decoded-object LRU. Sized to comfortably
/// hold the working set of a history walk (commits/trees/blobs and their delta
/// bases) without growing without bound on large repositories. Overridable via the
/// `SLEY_OBJECT_CACHE_BYTES` environment variable; there is currently no git-config
/// hook threaded into the object database, so this constant is the default.
const DEFAULT_OBJECT_CACHE_BYTES: usize = 96 * 1024 * 1024;

/// Default approximate byte budget for each per-pack delta-base cache. Holds the
/// decoded bases of the delta chains being walked so neighboring reads stay warm.
/// Overridable via `SLEY_DELTA_BASE_CACHE_BYTES`.
const DEFAULT_DELTA_BASE_CACHE_BYTES: usize = 96 * 1024 * 1024;

/// Approximate heap cost of caching one [`EncodedObject`]: its body plus a fixed
/// allowance for the key, enum/`Vec` headers, and per-entry map overhead. Used
/// only to drive eviction, so an estimate is fine.
fn cached_object_cost(object: &EncodedObject) -> usize {
    object.body.len().saturating_add(64)
}

/// Read an approximate byte budget from `var`, falling back to `default` when the
/// variable is unset or unparseable. A value of `0` disables the cache.
fn cache_budget_from_env(var: &str, default: usize) -> usize {
    match env::var(var) {
        Ok(value) => value.trim().parse::<usize>().unwrap_or(default),
        Err(_) => default,
    }
}

/// Approximate byte budget for the decoded-object LRU (see
/// [`DEFAULT_OBJECT_CACHE_BYTES`], `SLEY_OBJECT_CACHE_BYTES`).
///
/// Resolved once per process: the environment does not change under us, and a new
/// `FileObjectDatabase` is built often enough (e.g. once per revision resolved)
/// that re-reading the variable each time showed up as per-object overhead.
fn object_cache_budget() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        cache_budget_from_env("SLEY_OBJECT_CACHE_BYTES", DEFAULT_OBJECT_CACHE_BYTES)
    })
}

/// Approximate byte budget for each per-pack delta-base cache (see
/// [`DEFAULT_DELTA_BASE_CACHE_BYTES`], `SLEY_DELTA_BASE_CACHE_BYTES`). Resolved
/// once per process for the same reason as [`object_cache_budget`].
fn delta_base_cache_budget() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        cache_budget_from_env(
            "SLEY_DELTA_BASE_CACHE_BYTES",
            DEFAULT_DELTA_BASE_CACHE_BYTES,
        )
    })
}

/// Whether to re-hash every object on read and compare it to the requested id.
///
/// Off by default, matching git: reads trust the pack index → offset mapping and
/// the loose object's on-disk name, and object ids are verified where git verifies
/// them — when a pack is received (the index build re-hashes every object) and on
/// demand via [`FileObjectDatabase`]'s `validate`/fsck. Re-hashing on *every* read
/// dominated bulk-read cost (a scalar pure-Rust SHA-1 over each object's full
/// body), so it is opt-in via `SLEY_VERIFY_READS` (any value other than unset, ``,
/// or `0`) for callers that want the paranoid check back. Read once and cached, so
/// the default path pays only a single relaxed atomic load per read.
fn verify_reads_enabled() -> bool {
    static VERIFY: OnceLock<bool> = OnceLock::new();
    *VERIFY.get_or_init(|| match env::var("SLEY_VERIFY_READS") {
        Ok(value) => !matches!(value.trim(), "" | "0"),
        Err(_) => false,
    })
}

/// A memory-capped LRU map from a key `K` to a decoded [`EncodedObject`].
///
/// Eviction is by approximate byte budget (gix-style), not object count, so the
/// cache adapts to object size. On access an entry is moved to most-recently-used;
/// on insert, least-recently-used entries are dropped until the budget holds. A
/// budget of `0` makes the cache inert. Generic over the key so it backs both the
/// oid-keyed decoded-object cache and the offset-keyed delta-base cache.
#[derive(Debug)]
struct LruCache<K: std::hash::Hash + Eq + Clone> {
    budget: usize,
    used: usize,
    map: HashMap<K, Arc<EncodedObject>>,
    order: VecDeque<K>,
}

impl<K: std::hash::Hash + Eq + Clone> LruCache<K> {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &K) -> Option<Arc<EncodedObject>> {
        let object = Arc::clone(self.map.get(key)?);
        self.touch(key);
        Some(object)
    }

    /// Move `key` to the most-recently-used end. Linear in the recency queue, but
    /// the queue is bounded by the byte budget and this only runs on cache hits.
    fn touch(&mut self, key: &K) {
        if let Some(position) = self.order.iter().position(|existing| existing == key)
            && let Some(found) = self.order.remove(position)
        {
            self.order.push_back(found);
        }
    }

    /// Drop `key` from both the map and the recency queue, releasing its budget.
    fn remove(&mut self, key: &K) {
        if let Some(object) = self.map.remove(key) {
            self.used = self.used.saturating_sub(cached_object_cost(&object));
        }
        if let Some(position) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(position);
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.used = 0;
    }

    fn put(&mut self, key: K, object: Arc<EncodedObject>) {
        if self.budget == 0 {
            return;
        }
        let cost = cached_object_cost(&object);
        // A single object larger than the whole budget is not worth caching; it
        // would immediately evict everything including itself. Drop any stale
        // smaller entry stored under the same key so accounting stays exact.
        if cost > self.budget {
            self.remove(&key);
            return;
        }
        if let Some(previous) = self.map.insert(key.clone(), object) {
            // Replacing an existing entry: adjust accounting and refresh recency.
            self.used = self
                .used
                .saturating_sub(cached_object_cost(&previous))
                .saturating_add(cost);
            self.touch(&key);
        } else {
            self.used = self.used.saturating_add(cost);
            self.order.push_back(key);
        }
        while self.used > self.budget {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            if let Some(object) = self.map.remove(&evicted) {
                self.used = self.used.saturating_sub(cached_object_cost(&object));
            }
        }
    }
}

/// Decoded-object cache keyed by object id (loose + packed reads share it).
type LruObjectCache = LruCache<ObjectId>;
/// Delta-base cache keyed by in-pack byte offset, scoped to one pack.
type LruOffsetCache = LruCache<u64>;

/// Bridges the offset-keyed [`LruOffsetCache`] to [`sley_pack::PackDeltaCache`]
/// so the pack decoder can reuse decoded delta bases. Holds the shared cache
/// behind its mutex; a poisoned lock simply behaves as a cache miss/no-op, so a
/// decode still completes correctly (just without reuse).
struct PackDeltaCacheAdapter<'a>(&'a Arc<Mutex<LruOffsetCache>>);

impl sley_pack::PackDeltaCache for PackDeltaCacheAdapter<'_> {
    fn get(&self, offset: u64) -> Option<Arc<EncodedObject>> {
        self.0.lock().ok()?.get(&offset)
    }

    fn insert(&self, offset: u64, object: Arc<EncodedObject>) {
        if let Ok(mut cache) = self.0.lock() {
            cache.put(offset, object);
        }
    }
}

/// Parsed pack indexes keyed by `.idx` path, shared across cloned handles. Caches
/// the index parse so locating a packed object doesn't re-parse every `.idx` on
/// each read.
type PackIndexCache = Arc<Mutex<HashMap<PathBuf, Arc<PackIndex>>>>;

/// Parsed multi-pack-index files keyed by path, shared across cloned handles.
/// Caches the MIDX parse so object lookups in repositories with a MIDX avoid
/// reparsing the same fanout/object tables for every read.
type MultiPackIndexCache = Arc<Mutex<HashMap<PathBuf, Arc<MultiPackIndex>>>>;

/// A `.idx`/`.pack` pair discovered in a pack directory.
#[derive(Debug, Clone)]
struct DiscoveredPack {
    idx: PathBuf,
    pack: PathBuf,
}

/// The discovered `.idx`/`.pack` pairs in each pack directory, keyed by the pack
/// directory and shared across cloned handles. Caches the directory scan so a
/// bulk read (e.g. `cat-file --batch`) does not `read_dir` the pack directory on
/// every object lookup. New packs are still found: a lookup that misses every
/// cached pack re-scans the directory once before concluding the object is absent
/// (see [`FileObjectDatabase::find_pack_containing`]).
type PackListingCache = Arc<Mutex<HashMap<PathBuf, Arc<Vec<DiscoveredPack>>>>>;

#[derive(Debug, Clone)]
pub struct FileObjectDatabase {
    loose: LooseObjectStore,
    objects_dir: PathBuf,
    alternates: Vec<PathBuf>,
    format: ObjectFormat,
    pack_bytes: PackBytesCache,
    pack_indexes: PackIndexCache,
    multi_pack_indexes: MultiPackIndexCache,
    pack_listing: PackListingCache,
    decoded: DecodedObjectCache,
    pack_deltas: PackDeltaCaches,
}

pub fn repository_objects_dir(git_dir: impl AsRef<Path>) -> PathBuf {
    env::var_os("GIT_OBJECT_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| repository_common_dir(git_dir).join("objects"))
}

pub fn repository_common_dir(git_dir: impl AsRef<Path>) -> PathBuf {
    if let Some(common_dir) = env::var_os("GIT_COMMON_DIR") {
        return PathBuf::from(common_dir);
    }
    let git_dir = git_dir.as_ref();
    let commondir = git_dir.join("commondir");
    if let Ok(value) = fs::read_to_string(&commondir) {
        let path = PathBuf::from(value.trim());
        let common = if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        };
        return fs::canonicalize(&common).unwrap_or(common);
    }
    git_dir.to_path_buf()
}

pub fn repository_object_ids(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    object_ids_in_objects_dir(repository_objects_dir(git_dir), format)
}

pub fn object_ids_in_objects_dir(
    objects_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let objects_dir = objects_dir.as_ref();
    let mut oids = HashSet::new();
    collect_loose_object_ids(objects_dir, format, &mut oids)?;
    collect_packed_object_ids(&objects_dir.join("pack"), format, &mut oids)?;
    let mut oids = oids.into_iter().collect::<Vec<_>>();
    oids.sort_by_key(ObjectId::to_hex);
    Ok(oids)
}

fn collect_loose_object_ids(
    objects_dir: &Path,
    format: ObjectFormat,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !objects_dir.exists() {
        return Ok(());
    }
    let hex_len = format.hex_len();
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(fanout) = name.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for object_entry in fs::read_dir(entry.path())? {
            let object_entry = object_entry?;
            if !object_entry.file_type()?.is_file() {
                continue;
            }
            let name = object_entry.file_name();
            let Some(suffix) = name.to_str() else {
                continue;
            };
            if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            oids.insert(ObjectId::from_hex(format, &format!("{fanout}{suffix}"))?);
        }
    }
    Ok(())
}

fn collect_packed_object_ids(
    pack_dir: &Path,
    format: ObjectFormat,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !pack_dir.exists() {
        return Ok(());
    }
    let midx_path = pack_dir.join("multi-pack-index");
    if midx_path.exists() {
        let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, format)?;
        oids.extend(midx.objects.into_iter().map(|entry| entry.oid));
    }
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let index = PackIndex::parse(&fs::read(path)?, format)?;
        oids.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(())
}

impl FileObjectDatabase {
    pub fn new(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        let objects_dir = objects_dir.into();
        Self {
            loose: LooseObjectStore::new(objects_dir.clone(), format),
            alternates: alternate_object_dirs(&objects_dir),
            objects_dir,
            format,
            pack_bytes: Arc::new(Mutex::new(HashMap::new())),
            pack_indexes: Arc::new(Mutex::new(HashMap::new())),
            multi_pack_indexes: Arc::new(Mutex::new(HashMap::new())),
            pack_listing: Arc::new(Mutex::new(HashMap::new())),
            decoded: Arc::new(Mutex::new(LruObjectCache::new(object_cache_budget()))),
            pack_deltas: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn without_alternates(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        let objects_dir = objects_dir.into();
        Self {
            loose: LooseObjectStore::new(objects_dir.clone(), format),
            alternates: Vec::new(),
            objects_dir,
            format,
            pack_bytes: Arc::new(Mutex::new(HashMap::new())),
            pack_indexes: Arc::new(Mutex::new(HashMap::new())),
            multi_pack_indexes: Arc::new(Mutex::new(HashMap::new())),
            pack_listing: Arc::new(Mutex::new(HashMap::new())),
            decoded: Arc::new(Mutex::new(LruObjectCache::new(object_cache_budget()))),
            pack_deltas: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        Self::new(repository_objects_dir(git_dir), format)
    }

    /// Drop cached pack listings, indexes, and decoded objects so the next read
    /// sees packs/objects installed after this handle was created (e.g. after
    /// `fetch` or `install_pack`). Long-lived [`Repository`] sessions call this
    /// via the owning repository's `refresh_objects` hook.
    pub fn refresh_read_cache(&self) {
        if let Ok(mut cache) = self.pack_listing.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.pack_indexes.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.multi_pack_indexes.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.pack_bytes.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.pack_deltas.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.decoded.lock() {
            cache.clear();
        }
    }

    pub fn loose(&self) -> &LooseObjectStore {
        &self.loose
    }

    pub fn install_pack(&self, pack: &PackWrite) -> Result<PackInstallResult> {
        self.install_pack_with_options(pack, RawPackInstallOptions::default())
    }

    pub fn install_pack_with_options(
        &self,
        pack: &PackWrite,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        if pack.checksum.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "pack checksum uses {}, store uses {}",
                pack.checksum.format().name(),
                self.format.name()
            )));
        }
        for entry in &pack.entries {
            if entry.oid.format() != self.format {
                return Err(GitError::InvalidObjectId(format!(
                    "pack entry {} uses {}, store uses {}",
                    entry.oid,
                    entry.oid.format().name(),
                    self.format.name()
                )));
            }
        }
        let canonical_index = PackIndex::write_v2_for_pack(&pack.pack, self.format)?;
        let parsed_index = PackIndex::parse(&pack.index, self.format)?;
        if canonical_index.pack_checksum != pack.checksum
            || parsed_index.pack_checksum != pack.checksum
        {
            return Err(GitError::InvalidFormat(
                "pack and index checksums do not match pack write".into(),
            ));
        }
        if pack.index != canonical_index.index {
            return Err(GitError::InvalidFormat(
                "pack index does not match pack contents".into(),
            ));
        }

        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", pack.checksum.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        if !pack_path.exists() || !index_path.exists() {
            write_pack_component(&pack_path, &pack.pack)?;
            write_pack_component(&index_path, &pack.index)?;
        }
        let promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, options.promisor)?;
        Ok(PackInstallResult {
            pack_name,
            pack_path,
            index_path,
            promisor_path,
            object_ids: canonical_index
                .entries
                .iter()
                .map(|entry| entry.oid)
                .collect(),
        })
    }

    /// Install a pack that was produced in this process by [`PackFile::write_packed`].
    ///
    /// Unlike [`Self::install_raw_pack_with_options`], this does not re-inflate
    /// every pack entry to rebuild the index. It validates the generated pack
    /// trailer and generated index against the writer's object ids, CRCs, and
    /// offsets, then writes those bytes directly. Use the raw installer for
    /// arbitrary pack bytes received from an untrusted transport.
    pub fn install_written_pack(&self, pack: &PackWrite) -> Result<PackInstallResult> {
        self.install_written_pack_with_options(pack, RawPackInstallOptions::default())
    }

    pub fn install_written_pack_with_options(
        &self,
        pack: &PackWrite,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        validate_pack_checksum(&pack.pack, self.format, &pack.checksum, "pack write")?;
        let parsed_index = PackIndex::parse(&pack.index, self.format)?;
        if parsed_index.pack_checksum != pack.checksum {
            return Err(GitError::InvalidFormat(
                "pack write index checksum does not match pack".into(),
            ));
        }
        if !pack_index_entries_match_writer(&parsed_index.entries, &pack.entries) {
            return Err(GitError::InvalidFormat(
                "pack write index does not match generated entries".into(),
            ));
        }
        self.install_generated_pack_unchecked(pack, options)
    }

    fn install_generated_pack_unchecked(
        &self,
        pack: &PackWrite,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", pack.checksum.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        if !pack_path.exists() || !index_path.exists() {
            write_pack_component(&pack_path, &pack.pack)?;
            write_pack_component(&index_path, &pack.index)?;
        }
        let promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, options.promisor)?;
        Ok(PackInstallResult {
            pack_name,
            pack_path,
            index_path,
            promisor_path,
            object_ids: pack.entries.iter().map(|entry| entry.oid).collect(),
        })
    }

    pub fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<PackInstallResult> {
        self.install_raw_pack_with_options(pack_bytes, RawPackInstallOptions::default())
    }

    pub fn install_raw_pack_with_options(
        &self,
        pack_bytes: &[u8],
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        let built = PackIndex::write_v2_for_pack(pack_bytes, self.format)?;
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", built.pack_checksum.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        if !pack_path.exists() || !index_path.exists() {
            write_pack_component(&pack_path, pack_bytes)?;
            write_pack_component(&index_path, &built.index)?;
        }
        let promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, options.promisor)?;
        Ok(PackInstallResult {
            pack_name,
            pack_path,
            index_path,
            promisor_path,
            object_ids: built.entries.iter().map(|entry| entry.oid).collect(),
        })
    }

    pub fn contains(&self, oid: &ObjectId) -> Result<bool> {
        if self.loose.exists(oid)? {
            return Ok(true);
        }
        if self.find_pack_containing(oid)?.is_some() {
            return Ok(true);
        }
        for alternate in &self.alternates {
            if Self::without_alternates(alternate, self.format).contains(oid)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        let mut oids = object_ids_in_objects_dir(&self.objects_dir, self.format)?
            .into_iter()
            .collect::<HashSet<_>>();
        for alternate in &self.alternates {
            oids.extend(Self::without_alternates(alternate, self.format).object_ids()?);
        }
        let mut oids = oids.into_iter().collect::<Vec<_>>();
        oids.sort_by_key(ObjectId::to_hex);
        Ok(oids)
    }

    pub fn object_storage_info(&self, oid: &ObjectId) -> Result<Option<ObjectStorageInfo>> {
        if let Some(disk_size) = self.loose.disk_size(oid)? {
            return Ok(Some(ObjectStorageInfo {
                disk_size,
                deltabase: zero_oid(self.format)?,
            }));
        }
        if let Some(info) = self.packed_object_storage_info(oid)? {
            return Ok(Some(info));
        }
        for alternate in &self.alternates {
            if let Some(info) =
                Self::without_alternates(alternate, self.format).object_storage_info(oid)?
            {
                return Ok(Some(info));
            }
        }
        Ok(None)
    }

    pub fn resolve_prefix(&self, prefix: &str) -> Result<ObjectPrefixResolution> {
        validate_object_id_prefix(self.format, prefix)?;
        let mut matches = Vec::new();
        for oid in self.object_ids()? {
            if object_id_matches_prefix(&oid, prefix) {
                matches.push(oid);
            }
        }
        Ok(match matches.len() {
            0 => ObjectPrefixResolution::Missing,
            1 => ObjectPrefixResolution::Unique(matches.remove(0)),
            _ => ObjectPrefixResolution::Ambiguous(matches),
        })
    }

    /// The object type and content size of `oid` without decoding its full body —
    /// git's `cat-file --batch-check` fast path. Tries the decoded-object cache,
    /// then loose storage (inflating only the framing header), then packs (reading
    /// the entry header and, for deltas, only the delta's leading varints), then
    /// alternates. Returns `Ok(None)` if the object is not present.
    ///
    /// Unlike [`ObjectReader::read_object`], this never materializes the body, so it
    /// stays cheap on huge blobs and deep delta chains. It does not populate the
    /// decoded-object cache (nothing is decoded).
    pub fn read_object_header(&self, oid: &ObjectId) -> Result<Option<(ObjectType, u64)>> {
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(oid)
        {
            return Ok(Some((object.object_type, object.body.len() as u64)));
        }
        if let Some(header) = self.loose.read_header(oid)? {
            return Ok(Some(header));
        }
        if let Some(pack_paths) = self.find_pack_containing(oid)? {
            let bytes = self.cached_pack_bytes(&pack_paths.pack)?;
            let header =
                sley_pack::read_object_header_at(&bytes, pack_paths.offset, self.format, |base| {
                    self.read_object_header(base)
                        .map(|header| header.map(|(t, _)| t))
                })?;
            return Ok(Some(header));
        }
        for alternate in &self.alternates {
            if let Some(header) =
                Self::without_alternates(alternate, self.format).read_object_header(oid)?
            {
                return Ok(Some(header));
            }
        }
        Ok(None)
    }

    fn read_packed_object(&self, oid: &ObjectId) -> Result<Option<Arc<EncodedObject>>> {
        // Memory-capped decoded-object cache first (delta-base reuse for ref-delta
        // bases that resolve back through the store + repeated whole-object reads).
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(oid)
        {
            return Ok(Some(object));
        }
        let Some(pack_paths) = self.find_pack_containing(oid)? else {
            return Ok(None);
        };
        let bytes = self.cached_pack_bytes(&pack_paths.pack)?;
        // Per-pack delta-base cache (keyed by in-pack offset). Resolving an
        // ofs-delta chain reuses already-decoded bases instead of re-inflating the
        // whole chain on every read. Scoped to this pack's path so an offset key is
        // never applied to the wrong pack's bytes.
        let delta_cache = self.pack_delta_cache(&pack_paths.pack);
        let delta_adapter = delta_cache.as_ref().map(PackDeltaCacheAdapter);
        // Decode only this object at its offset (plus its delta-base chain). A
        // ref-delta base resolves through the full store (loose / other packs) and
        // reuses the decoded-object cache. No cache lock is held across the decode,
        // so the recursive resolver re-entry (which may re-enter read_object) is
        // safe.
        let resolve_ref_base = |base: &ObjectId| self.read_object(base).map(Some);
        let object = match &delta_adapter {
            Some(adapter) => sley_pack::read_object_at_with_cache_arc(
                &bytes,
                pack_paths.offset,
                self.format,
                resolve_ref_base,
                adapter,
            )?,
            None => sley_pack::read_object_at_arc(
                &bytes,
                pack_paths.offset,
                self.format,
                resolve_ref_base,
            )?,
        };
        // Trust the index → offset mapping rather than re-hashing every decoded
        // object on read (see `verify_reads_enabled`); this re-hash dominated
        // bulk-read cost. Opt back in with `SLEY_VERIFY_READS` for a paranoid check.
        if verify_reads_enabled() {
            let actual = object.object_id(self.format)?;
            if actual != *oid {
                return Err(GitError::InvalidObject(format!(
                    "pack object id mismatch: index says {oid}, decoded {actual}"
                )));
            }
        }
        if let Ok(mut cache) = self.decoded.lock() {
            cache.put(*oid, Arc::clone(&object));
        }
        Ok(Some(object))
    }

    /// The per-pack delta-base cache for `pack_path`, creating it on first use.
    /// Returns `None` only if the shared map's lock is poisoned, in which case the
    /// caller falls back to an uncached decode (correctness preserved).
    fn pack_delta_cache(&self, pack_path: &Path) -> Option<Arc<Mutex<LruOffsetCache>>> {
        let mut caches = self.pack_deltas.lock().ok()?;
        let cache = caches.entry(pack_path.to_path_buf()).or_insert_with(|| {
            Arc::new(Mutex::new(LruOffsetCache::new(delta_base_cache_budget())))
        });
        Some(Arc::clone(cache))
    }

    /// Backing bytes of the pack at `pack_path`, loaded at most once per database
    /// handle (cached, shared across clones). Memory-mapped under the `mmap` feature,
    /// otherwise read into the heap. On a poisoned lock it falls back to loading
    /// without caching, preserving correctness.
    fn cached_pack_bytes(&self, pack_path: &Path) -> Result<Arc<PackData>> {
        if let Ok(cache) = self.pack_bytes.lock()
            && let Some(bytes) = cache.get(pack_path)
        {
            return Ok(Arc::clone(bytes));
        }
        let bytes = Arc::new(load_pack_data(pack_path)?);
        if let Ok(mut cache) = self.pack_bytes.lock() {
            cache.insert(pack_path.to_path_buf(), Arc::clone(&bytes));
        }
        Ok(bytes)
    }

    /// Parsed index for the `.idx` at `index_path`, parsed at most once per
    /// database handle. On a poisoned lock it falls back to parsing without
    /// caching, preserving correctness.
    fn cached_pack_index(&self, index_path: &Path) -> Result<Arc<PackIndex>> {
        if let Ok(cache) = self.pack_indexes.lock()
            && let Some(index) = cache.get(index_path)
        {
            return Ok(Arc::clone(index));
        }
        let index = Arc::new(PackIndex::parse(&fs::read(index_path)?, self.format)?);
        if let Ok(mut cache) = self.pack_indexes.lock() {
            cache.insert(index_path.to_path_buf(), Arc::clone(&index));
        }
        Ok(index)
    }

    /// Parsed multi-pack-index at `midx_path`, parsed at most once per database
    /// handle. Returns `Ok(None)` when no MIDX exists. On a poisoned lock it
    /// falls back to parsing without caching, preserving correctness.
    fn cached_multi_pack_index(&self, midx_path: &Path) -> Result<Option<Arc<MultiPackIndex>>> {
        if !midx_path.exists() {
            return Ok(None);
        }
        if let Ok(cache) = self.multi_pack_indexes.lock()
            && let Some(midx) = cache.get(midx_path)
        {
            return Ok(Some(Arc::clone(midx)));
        }
        let midx = Arc::new(MultiPackIndex::parse(&fs::read(midx_path)?, self.format)?);
        if let Ok(mut cache) = self.multi_pack_indexes.lock() {
            cache.insert(midx_path.to_path_buf(), Arc::clone(&midx));
        }
        Ok(Some(midx))
    }

    /// The discovered `.idx`/`.pack` pairs in `pack_dir`, cached and shared across
    /// clones. With `force_rescan`, the directory is re-read; the freshly scanned
    /// listing is only stored (and returned as a new `Arc`) when its set of `.idx`
    /// files actually differs from the cached one, so an unchanged directory keeps
    /// the same `Arc` (letting callers detect "nothing new" cheaply). On a poisoned
    /// lock it scans without caching, preserving correctness.
    fn cached_pack_listing(
        &self,
        pack_dir: &Path,
        force_rescan: bool,
    ) -> Result<Arc<Vec<DiscoveredPack>>> {
        if !force_rescan
            && let Ok(cache) = self.pack_listing.lock()
            && let Some(listing) = cache.get(pack_dir)
        {
            return Ok(Arc::clone(listing));
        }
        let scanned = Arc::new(scan_pack_listing(pack_dir)?);
        if let Ok(mut cache) = self.pack_listing.lock() {
            match cache.get(pack_dir) {
                // Keep the existing Arc when the scan found the same set of packs,
                // so repeated misses don't churn the cache or callers' pointers.
                Some(existing) if same_pack_set(existing, &scanned) => {
                    return Ok(Arc::clone(existing));
                }
                _ => {
                    cache.insert(pack_dir.to_path_buf(), Arc::clone(&scanned));
                }
            }
        }
        Ok(scanned)
    }

    /// Find `oid` among a cached pack listing, returning its pack path and offset.
    /// Uses the parsed-index cache, so this performs no directory I/O.
    fn find_in_pack_listing(
        &self,
        listing: &[DiscoveredPack],
        oid: &ObjectId,
    ) -> Result<Option<PackPaths>> {
        for pack in listing {
            let index = self.cached_pack_index(&pack.idx)?;
            if let Some(entry) = index.find(oid) {
                return Ok(Some(PackPaths {
                    pack: pack.pack.clone(),
                    offset: entry.offset,
                }));
            }
        }
        Ok(None)
    }

    fn find_pack_containing(&self, oid: &ObjectId) -> Result<Option<PackPaths>> {
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        let pack_dir = self.objects_dir.join("pack");
        if !pack_dir.exists() {
            return Ok(None);
        }
        if let Some(pack_paths) = self.find_midx_pack_containing(&pack_dir, oid)? {
            return Ok(Some(pack_paths));
        }
        // Search the cached directory listing first. On a complete miss, re-scan
        // the directory once (picking up any pack added since the listing was
        // cached) and search again, so newly written packs are still found.
        let listing = self.cached_pack_listing(&pack_dir, false)?;
        if let Some(pack_paths) = self.find_in_pack_listing(&listing, oid)? {
            return Ok(Some(pack_paths));
        }
        let refreshed = self.cached_pack_listing(&pack_dir, true)?;
        if Arc::ptr_eq(&listing, &refreshed) {
            // The re-scan produced the same listing, so nothing new appeared.
            return Ok(None);
        }
        self.find_in_pack_listing(&refreshed, oid)
    }

    fn packed_object_storage_info(&self, oid: &ObjectId) -> Result<Option<ObjectStorageInfo>> {
        let Some(pack_paths) = self.find_pack_containing(oid)? else {
            return Ok(None);
        };
        let pack_len = fs::metadata(&pack_paths.pack)?.len();
        let trailer_offset = pack_len
            .checked_sub(self.format.raw_len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack file shorter than checksum".into()))?;
        let index_path = pack_paths.pack.with_extension("idx");
        let index = self.cached_pack_index(&index_path)?;
        let pack = self.cached_pack_bytes(&pack_paths.pack)?;
        let delta_base = pack_entry_delta_base(self.format, &pack, pack_paths.offset)?;
        let delta_base_offset = match &delta_base {
            Some(PackDeltaBase::Offset(offset)) => Some(*offset),
            Some(PackDeltaBase::Ref(_)) | None => None,
        };
        let offset_info =
            scan_pack_index_offsets(&index, pack_paths.offset, trailer_offset, delta_base_offset)?;
        let disk_size = offset_info
            .end_offset
            .checked_sub(pack_paths.offset)
            .ok_or_else(|| GitError::InvalidFormat("pack index offsets are not sorted".into()))?;
        let deltabase = match delta_base {
            Some(PackDeltaBase::Offset(_)) => offset_info
                .delta_base_oid
                .expect("scan_pack_index_offsets validates ofs-delta base offsets"),
            Some(PackDeltaBase::Ref(oid)) => oid,
            None => zero_oid(self.format)?,
        };
        Ok(Some(ObjectStorageInfo {
            disk_size,
            deltabase,
        }))
    }

    fn find_midx_pack_containing(
        &self,
        pack_dir: &Path,
        oid: &ObjectId,
    ) -> Result<Option<PackPaths>> {
        let midx_path = pack_dir.join("multi-pack-index");
        let Some(midx) = self.cached_multi_pack_index(&midx_path)? else {
            return Ok(None);
        };
        let Some(entry) = midx.find(oid) else {
            return Ok(None);
        };
        let Some(pack_name) = midx.pack_names.get(entry.pack_int_id as usize) else {
            return Err(GitError::InvalidFormat(
                "multi-pack-index object points past pack table".into(),
            ));
        };
        let pack_file_name = pack_name
            .strip_suffix(".idx")
            .map(|stem| format!("{stem}.pack"))
            .unwrap_or_else(|| pack_name.clone());
        let pack = pack_dir.join(pack_file_name);
        if !pack.exists() {
            return Err(GitError::not_found(format!(
                "pack file {} for multi-pack-index {}",
                pack.display(),
                midx_path.display()
            )));
        }
        Ok(Some(PackPaths {
            pack,
            offset: entry.offset,
        }))
    }
}

fn validate_object_id_prefix(format: ObjectFormat, prefix: &str) -> Result<()> {
    if prefix.len() < 4 || prefix.len() > format.hex_len() {
        return Err(GitError::InvalidObjectId(format!(
            "expected 4 to {} hex digits for {}, got {}",
            format.hex_len(),
            format.name(),
            prefix.len()
        )));
    }
    if !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitError::InvalidObjectId(format!(
            "non-hex object id prefix {prefix}"
        )));
    }
    Ok(())
}

fn object_id_matches_prefix(oid: &ObjectId, prefix: &str) -> bool {
    oid.to_hex()
        .as_bytes()
        .iter()
        .zip(prefix.as_bytes())
        .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
}

/// Scan `pack_dir` for `.idx` files that have a matching `.pack` sibling,
/// returning the discovered pairs. An `.idx` without its `.pack` is skipped (an
/// orphan index cannot serve objects), matching the prior per-read behavior.
fn scan_pack_listing(pack_dir: &Path) -> Result<Vec<DiscoveredPack>> {
    let mut packs = Vec::new();
    for entry in fs::read_dir(pack_dir)? {
        let entry = entry?;
        let idx = entry.path();
        if idx.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let Some(stem) = idx.file_stem() else {
            continue;
        };
        let pack = idx.with_file_name(format!("{}.pack", stem.to_string_lossy()));
        if !pack.exists() {
            continue;
        }
        packs.push(DiscoveredPack { idx, pack });
    }
    // Deterministic order so lookups and set comparison are stable.
    packs.sort_by(|left, right| left.idx.cmp(&right.idx));
    Ok(packs)
}

/// Whether two pack listings reference the same set of `.idx` files (order is
/// already normalized by [`scan_pack_listing`]).
fn same_pack_set(left: &[DiscoveredPack], right: &[DiscoveredPack]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(a, b)| a.idx == b.idx && a.pack == b.pack)
}

fn alternate_object_dirs(objects_dir: &Path) -> Vec<PathBuf> {
    let mut alternates = Vec::new();
    if let Some(value) = env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
        for raw in value.to_string_lossy().split(':') {
            if !raw.is_empty() {
                alternates.push(PathBuf::from(raw));
            }
        }
    }
    let alternates_path = objects_dir.join("info").join("alternates");
    if let Ok(contents) = fs::read(&alternates_path) {
        for raw in contents.split(|byte| *byte == b'\n') {
            let line = raw.strip_suffix(b"\r").unwrap_or(raw);
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            let Ok(value) = std::str::from_utf8(line) else {
                continue;
            };
            let path = Path::new(value);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                objects_dir.join(path)
            };
            alternates.push(absolute);
        }
    }
    alternates
}

impl ObjectReader for FileObjectDatabase {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        match self.loose.read_object(oid) {
            Ok(object) => return Ok(object),
            Err(GitError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
        if let Some(object) = self.read_packed_object(oid)? {
            return Ok(object);
        }
        for alternate in &self.alternates {
            match Self::without_alternates(alternate, self.format).read_object(oid) {
                Ok(object) => return Ok(object),
                Err(GitError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Err(GitError::object_not_found(*oid))
    }
}

impl ObjectWriter for FileObjectDatabase {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId> {
        // Mirror git's freshen semantics (`write_object_file`:
        // `freshen_packed_object || freshen_loose_object`): an object already
        // present anywhere in the database — loose, packed, or through an
        // alternate — is not written again, so e.g. `git add` after
        // `git repack -ad` does not resurrect a loose copy of a packed object.
        let oid = object.object_id(self.format)?;
        if self.contains(&oid)? {
            return Ok(oid);
        }
        self.loose.write_object(object)
    }
}

#[derive(Debug, Clone)]
struct PackPaths {
    pack: PathBuf,
    offset: u64,
}

fn write_pack_component(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| GitError::InvalidPath("pack component path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temp_path = unique_temp_path(parent);
    let write_result = (|| -> Result<()> {
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        match fs::rename(&temp_path, path) {
            Ok(()) => Ok(()),
            Err(_) if path.exists() => {
                let _ = fs::remove_file(&temp_path);
                Ok(())
            }
            Err(err) => Err(GitError::Io(err.to_string())),
        }
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn write_promisor_pack_sidecar(
    pack_dir: &Path,
    pack_name: &str,
    promisor: bool,
) -> Result<Option<PathBuf>> {
    if !promisor {
        return Ok(None);
    }
    let path = pack_dir.join(format!("{pack_name}.promisor"));
    write_pack_component(&path, b"")?;
    Ok(Some(path))
}

/// Maximum number of bytes git will inflate when reading a loose object's
/// `"<type> <size>\0"` header (git's `MAX_HEADER_LEN` in object-file.c). The NUL
/// terminator must land within this window, so a header of 32 or more non-NUL
/// bytes is rejected as too long.
const MAX_LOOSE_HEADER_LEN: usize = 32;

/// git's exact `error:`-level diagnostic for a loose object whose header overflows
/// `MAX_LOOSE_HEADER_LEN` (object-file.c: `error(_("header for %s too long, exceeds
/// %d bytes"), ...)`). Shared by the header-only and full-read paths so both surface
/// byte-identical text.
fn loose_header_too_long(oid: &ObjectId) -> GitError {
    GitError::InvalidObject(format!(
        "header for {oid} too long, exceeds {MAX_LOOSE_HEADER_LEN} bytes"
    ))
}

#[derive(Debug, Clone)]
pub struct LooseObjectStore {
    objects_dir: PathBuf,
    format: ObjectFormat,
}

impl LooseObjectStore {
    pub fn new(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        Self {
            objects_dir: objects_dir.into(),
            format,
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        Self::new(repository_objects_dir(git_dir), format)
    }

    pub fn object_path(&self, oid: &ObjectId) -> Result<PathBuf> {
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        let hex = oid.to_hex();
        Ok(self.objects_dir.join(&hex[..2]).join(&hex[2..]))
    }

    pub fn exists(&self, oid: &ObjectId) -> Result<bool> {
        Ok(self.object_path(oid)?.exists())
    }

    pub fn disk_size(&self, oid: &ObjectId) -> Result<Option<u64>> {
        match fs::metadata(self.object_path(oid)?) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(GitError::Io(err.to_string())),
        }
    }

    /// The object type and content size of `oid` from loose storage, inflating only
    /// the framing header (`"<type> <size>\0"`) and not the body. Output-limited
    /// reads keep miniz from inflating past the header even for large objects.
    /// Returns `Ok(None)` when the loose object is absent.
    pub fn read_header(&self, oid: &ObjectId) -> Result<Option<(ObjectType, u64)>> {
        let path = self.object_path(oid)?;
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        let mut decoder = ZlibDecoder::new(file);
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            // git inflates only the first `MAX_LOOSE_HEADER_LEN` bytes
            // (object-file.c `unpack_loose_header`) and reports ULHR_TOO_LONG when no
            // NUL terminator lands within them — whether the stream simply ends early
            // or overflows the window. Both collapse to the same `error:`-level
            // diagnostic, so a header that ends before its NUL is "too long" too.
            if decoder.read(&mut byte)? == 0 {
                return Err(loose_header_too_long(oid));
            }
            if byte[0] == 0 {
                break;
            }
            header.push(byte[0]);
            // A 31-byte header (NUL at the 32nd byte) is the longest that fits; 32
            // non-NUL bytes overflow the window.
            if header.len() >= MAX_LOOSE_HEADER_LEN {
                return Err(loose_header_too_long(oid));
            }
        }
        let header =
            std::str::from_utf8(&header).map_err(|err| GitError::InvalidObject(err.to_string()))?;
        let (kind, size) = header
            .split_once(' ')
            .ok_or_else(|| GitError::InvalidObject("missing object size".into()))?;
        let object_type = kind.parse::<ObjectType>()?;
        let size = size
            .parse::<u64>()
            .map_err(|_| GitError::InvalidObject("invalid object size".into()))?;
        Ok(Some((object_type, size)))
    }
}

impl ObjectReader for LooseObjectStore {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        let path = self.object_path(oid)?;
        let compressed = match fs::read(&path) {
            Ok(compressed) => compressed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(GitError::object_not_found(*oid));
            }
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut framed = Vec::new();
        decoder.read_to_end(&mut framed)?;
        // git only inflates the first `MAX_LOOSE_HEADER_LEN` bytes looking for the
        // header's NUL terminator before parsing the type; an over-long header is
        // rejected here (with git's diagnostic) rather than failing later as an
        // "unknown object type". Mirror that so `cat-file -p` matches upstream.
        if framed
            .iter()
            .take(MAX_LOOSE_HEADER_LEN)
            .all(|byte| *byte != 0)
        {
            return Err(loose_header_too_long(oid));
        }
        let object = parse_framed_object(&framed)?;
        // Trust the loose object's on-disk name rather than re-hashing its full body
        // on every read (see `verify_reads_enabled`); use `validate`/fsck or
        // `SLEY_VERIFY_READS` for an explicit integrity check.
        if verify_reads_enabled() {
            let actual = object.object_id(self.format)?;
            if &actual != oid {
                return Err(GitError::InvalidObject(format!(
                    "loose object {} hashes to {actual}",
                    path.display()
                )));
            }
        }
        Ok(Arc::new(object))
    }
}

impl ObjectWriter for LooseObjectStore {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        let path = self.object_path(&oid)?;
        if path.exists() {
            return Ok(oid);
        }
        let parent = path
            .parent()
            .ok_or_else(|| GitError::InvalidPath("loose object path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let temp_path = unique_temp_path(parent);
        let write_result = (|| -> Result<()> {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&object.framed_bytes())?;
            let compressed = encoder.finish()?;
            {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp_path)?;
                file.write_all(&compressed)?;
                file.sync_all()?;
            }
            match fs::rename(&temp_path, &path) {
                Ok(()) => Ok(()),
                Err(_) if path.exists() => {
                    let _ = fs::remove_file(&temp_path);
                    Ok(())
                }
                Err(err) => Err(GitError::Io(err.to_string())),
            }
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result?;
        Ok(oid)
    }
}

fn unique_temp_path(parent: &Path) -> PathBuf {
    let id = TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("tmp_obj_{}_{}", std::process::id(), id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::BString;
    use sley_object::{Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntry};
    use sley_pack::{PackFile, PackWriteOptions};

    fn blob_of(byte: u8, len: usize) -> EncodedObject {
        EncodedObject::new(ObjectType::Blob, vec![byte; len])
    }

    fn cached_blob_of(byte: u8, len: usize) -> Arc<EncodedObject> {
        Arc::new(blob_of(byte, len))
    }

    fn read_object_for_assert(reader: &impl ObjectReader, oid: &ObjectId) -> EncodedObject {
        reader
            .read_object(oid)
            .expect("test operation should succeed")
            .as_ref()
            .clone()
    }

    #[test]
    fn lru_cache_evicts_by_byte_budget_least_recently_used_first() {
        // Budget holds two ~1 KiB objects but not three.
        let one = cached_object_cost(&blob_of(0, 1000));
        let mut cache = LruCache::<u32>::new(one * 2 + 8);
        cache.put(1, cached_blob_of(b'a', 1000));
        cache.put(2, cached_blob_of(b'b', 1000));
        // Touch key 1 so key 2 becomes least-recently-used.
        assert!(cache.get(&1).is_some());
        cache.put(3, cached_blob_of(b'c', 1000));
        // Key 2 (LRU) is evicted; 1 and 3 remain.
        assert!(cache.get(&1).is_some());
        assert!(cache.get(&2).is_none());
        assert!(cache.get(&3).is_some());
    }

    #[test]
    fn lru_cache_zero_budget_is_inert() {
        let mut cache = LruCache::<u32>::new(0);
        cache.put(1, cached_blob_of(b'a', 16));
        assert!(cache.get(&1).is_none());
    }

    #[test]
    fn lru_cache_skips_object_larger_than_budget_and_clears_stale_entry() {
        let mut cache = LruCache::<u32>::new(cached_object_cost(&blob_of(0, 100)));
        cache.put(1, cached_blob_of(b'a', 50));
        assert!(cache.get(&1).is_some());
        // An object that cannot fit is not cached, and it evicts the prior entry
        // stored under the same key (so we never serve a stale value for it).
        cache.put(1, cached_blob_of(b'b', 10_000));
        assert!(cache.get(&1).is_none());
        // A subsequent fitting insert under another key still works and accounting
        // is not corrupted by the oversized insert.
        cache.put(2, cached_blob_of(b'c', 50));
        assert!(cache.get(&2).is_some());
    }

    #[test]
    fn lru_cache_replacing_entry_updates_byte_accounting() {
        // Budget holds two 500-byte objects (plus headroom) but not a 500 + a
        // ~1900-byte object.
        let small = cached_object_cost(&blob_of(0, 500));
        let mut cache = LruCache::<u32>::new(small * 2 + 200);
        cache.put(1, cached_blob_of(b'a', 500));
        cache.put(2, cached_blob_of(b'b', 500));
        assert!(cache.get(&1).is_some());
        assert!(cache.get(&2).is_some());
        // Replace key 2 (now MRU after the gets above re-ordered 1 then 2) with a
        // bigger value that still fits the budget alone but makes the running total
        // exceed it; the LRU (key 1) is evicted while the replaced key 2 stays.
        // This exercises the replace-path accounting.
        cache.put(2, cached_blob_of(b'b', 1000));
        assert!(cache.get(&2).is_some());
        assert!(cache.get(&1).is_none());
    }

    #[test]
    fn write_and_validate_blob() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .expect("test operation should succeed");
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        db.validate(&oid).expect("test operation should succeed");
    }

    #[test]
    fn loose_store_writes_and_reads_object() {
        let root = std::env::temp_dir().join(format!(
            "sley-loose-store-{}-{}",
            std::process::id(),
            TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut store = LooseObjectStore::new(root.join("objects"), ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec());
        let oid = store
            .write_object(object.clone())
            .expect("test operation should succeed");
        assert_eq!(read_object_for_assert(&store, &oid), object);
        assert!(
            store
                .object_path(&oid)
                .expect("test operation should succeed")
                .exists()
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_reads_object_from_pack_index() {
        let root = temp_root("sley-file-odb-pack");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"packed\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .expect("test operation should succeed");
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .expect("test operation should succeed");

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        assert!(db.contains(&oid).expect("test operation should succeed"));
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn read_object_header_matches_full_read_for_loose_and_packed_and_delta() {
        let root = temp_root("sley-read-object-header");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        // Loose object: the header read inflates only the framing, not the body.
        let loose = EncodedObject::new(ObjectType::Blob, b"loose header object\n".to_vec());
        let loose_oid = db
            .write_object(loose.clone())
            .expect("test operation should succeed");

        // Packed objects, including an ofs-delta whose *result* size lives in the
        // delta stream (not the pack entry header) and whose type is inherited from
        // its base at the end of the chain.
        let base = EncodedObject::new(ObjectType::Blob, vec![b'a'; 4096]);
        let mut child_body = vec![b'a'; 4096];
        child_body.extend_from_slice(b" plus a deltified tail\n");
        let child = EncodedObject::new(ObjectType::Blob, child_body);
        let commitish =
            EncodedObject::new(ObjectType::Commit, b"header-only type probe\n".to_vec());
        let base_oid = base
            .object_id(format)
            .expect("test operation should succeed");
        let child_oid = child
            .object_id(format)
            .expect("test operation should succeed");
        let commit_oid = commitish
            .object_id(format)
            .expect("test operation should succeed");
        let options = PackWriteOptions::new()
            .with_prefer_ofs_delta(true)
            .with_reorder(false);
        let pack = PackFile::write_packed_with_options(
            &[base.clone(), child.clone(), commitish.clone()],
            format,
            &options,
        )
        .expect("test operation should succeed");
        db.install_pack(&pack)
            .expect("test operation should succeed");

        // The header read agrees with a full decode for every object and storage
        // class, without ever materializing the body.
        for (oid, want_type, want_len) in [
            (&loose_oid, ObjectType::Blob, loose.body.len()),
            (&base_oid, ObjectType::Blob, base.body.len()),
            (&child_oid, ObjectType::Blob, child.body.len()),
            (&commit_oid, ObjectType::Commit, commitish.body.len()),
        ] {
            assert_eq!(
                db.read_object_header(oid)
                    .expect("test operation should succeed"),
                Some((want_type, want_len as u64)),
                "header for {oid}"
            );
            let full = db.read_object(oid).expect("test operation should succeed");
            assert_eq!(
                db.read_object_header(oid)
                    .expect("test operation should succeed"),
                Some((full.object_type, full.body.len() as u64))
            );
        }

        let missing = ObjectId::from_hex(format, "0000000000000000000000000000000000000001")
            .expect("test operation should succeed");
        assert_eq!(
            db.read_object_header(&missing)
                .expect("test operation should succeed"),
            None
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn object_storage_info_reports_loose_packed_and_delta_metadata() {
        let root = temp_root("sley-object-storage-info");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let loose = EncodedObject::new(ObjectType::Blob, b"loose storage object\n".to_vec());
        let loose_oid = db
            .write_object(loose)
            .expect("test operation should succeed");
        let loose_size = fs::metadata(
            db.loose()
                .object_path(&loose_oid)
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed")
        .len();
        let loose_info = db
            .object_storage_info(&loose_oid)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(loose_info.disk_size, loose_size);
        assert_eq!(
            loose_info.deltabase,
            zero_oid(format).expect("test operation should succeed")
        );

        let base = EncodedObject::new(ObjectType::Blob, vec![b'a'; 4096]);
        let mut child_body = vec![b'a'; 4096];
        child_body.extend_from_slice(b" changed tail\n");
        let child = EncodedObject::new(ObjectType::Blob, child_body);
        let base_oid = base
            .object_id(format)
            .expect("test operation should succeed");
        let child_oid = child
            .object_id(format)
            .expect("test operation should succeed");
        let options = PackWriteOptions::new()
            .with_prefer_ofs_delta(true)
            .with_reorder(false);
        let pack = PackFile::write_packed_with_options(&[base, child], format, &options)
            .expect("test operation should succeed");
        db.install_pack(&pack)
            .expect("test operation should succeed");

        let base_info = db
            .object_storage_info(&base_oid)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert!(base_info.disk_size > 0);
        assert_eq!(
            base_info.deltabase,
            zero_oid(format).expect("test operation should succeed")
        );

        let child_info = db
            .object_storage_info(&child_oid)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert!(child_info.disk_size > 0);
        assert_eq!(child_info.deltabase, base_oid);

        let missing = ObjectId::from_hex(format, "0000000000000000000000000000000000000001")
            .expect("test operation should succeed");
        assert_eq!(
            db.object_storage_info(&missing)
                .expect("test operation should succeed"),
            None
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_resolves_unique_loose_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-loose");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"prefix loose\n".to_vec());
        let oid = db
            .write_object(object)
            .expect("test operation should succeed");
        let prefix = &oid.to_hex()[..8];

        assert_eq!(
            db.resolve_prefix(prefix)
                .expect("test operation should succeed"),
            ObjectPrefixResolution::Unique(oid)
        );
        assert!(
            db.object_ids()
                .expect("test operation should succeed")
                .contains(&oid)
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_resolves_unique_packed_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-packed");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"prefix packed\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        db.install_pack(&pack)
            .expect("test operation should succeed");
        let prefix = &oid.to_hex()[..8];

        assert_eq!(
            db.resolve_prefix(prefix)
                .expect("test operation should succeed"),
            ObjectPrefixResolution::Unique(oid)
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_reports_ambiguous_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-ambiguous");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let mut seen = HashMap::new();
        let (prefix, first, second) = (0..10_000)
            .find_map(|idx| {
                let object =
                    EncodedObject::new(ObjectType::Blob, format!("ambiguous {idx}\n").into_bytes());
                let oid = db
                    .write_object(object)
                    .expect("test operation should succeed");
                let prefix = oid.to_hex()[..4].to_string();
                seen.insert(prefix.clone(), oid)
                    .map(|first| (prefix, first, oid))
            })
            .expect("test should find a 4-hex collision");

        let ObjectPrefixResolution::Ambiguous(mut matches) = db
            .resolve_prefix(&prefix)
            .expect("test operation should succeed")
        else {
            panic!("expected ambiguous prefix {prefix}");
        };
        matches.sort_by_key(ObjectId::to_hex);
        let mut expected = vec![first, second];
        expected.sort_by_key(ObjectId::to_hex);
        assert_eq!(matches, expected);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_rejects_too_short_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-short");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        assert!(matches!(
            db.resolve_prefix("abc"),
            Err(GitError::InvalidObjectId(_))
        ));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_reads_sha256_object_from_pack_index() {
        let root = temp_root("sley-file-odb-pack-sha256");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"packed sha256\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let written =
            PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
                .expect("test operation should succeed");
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .expect("test operation should succeed");
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .expect("test operation should succeed");

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);
        assert!(db.contains(&oid).expect("test operation should succeed"));
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_installs_sha256_pack_without_loose_objects() {
        let root = temp_root("sley-file-odb-install-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"installed sha256 pack\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);

        let result = db
            .install_pack(&pack)
            .expect("test operation should succeed");

        assert_eq!(result.pack_name, format!("pack-{}", pack.checksum.to_hex()));
        assert_eq!(result.object_ids, vec![oid]);
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert_eq!(result.promisor_path, None);
        assert!(
            !db.loose()
                .object_path(&oid)
                .expect("test operation should succeed")
                .exists()
        );
        assert!(db.contains(&oid).expect("test operation should succeed"));
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_installs_raw_sha256_pack_without_loose_objects() {
        let root = temp_root("sley-file-odb-install-raw-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"installed raw sha256 pack\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);

        let result = db
            .install_raw_pack(&pack.pack)
            .expect("test operation should succeed");

        assert_eq!(result.pack_name, format!("pack-{}", pack.checksum.to_hex()));
        assert_eq!(result.object_ids, vec![oid]);
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert_eq!(result.promisor_path, None);
        assert!(
            !db.loose()
                .object_path(&oid)
                .expect("test operation should succeed")
                .exists()
        );
        assert!(db.contains(&oid).expect("test operation should succeed"));
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_rejects_noncanonical_pack_index() {
        let root = temp_root("sley-file-odb-install-bad-index");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"bad index crc\n".to_vec());
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut entries = pack.entries.clone();
        entries[0].crc32 ^= 1;
        let mut bad_pack = pack.clone();
        bad_pack.index = PackIndex::write_v2(ObjectFormat::Sha1, &entries, &pack.checksum)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        assert!(db.install_pack(&bad_pack).is_err());

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_installs_raw_promisor_pack_with_sidecar() {
        let root = temp_root("sley-file-odb-install-raw-promisor-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"installed promisor pack\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let result = db
            .install_raw_pack_with_options(&pack.pack, RawPackInstallOptions { promisor: true })
            .expect("test operation should succeed");

        let promisor_path = result.promisor_path.expect("promisor sidecar");
        assert_eq!(promisor_path.file_stem(), result.pack_path.file_stem());
        assert_eq!(
            promisor_path.extension().and_then(|ext| ext.to_str()),
            Some("promisor")
        );
        assert!(promisor_path.exists());
        assert_eq!(
            fs::read(&promisor_path).expect("test operation should succeed"),
            b""
        );
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert!(
            !db.loose()
                .object_path(&oid)
                .expect("test operation should succeed")
                .exists()
        );
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn repository_objects_dir_uses_linked_worktree_common_dir() {
        let root = temp_root("sley-odb-common-dir");
        let common = root.join(".git");
        let admin = common.join("worktrees").join("linked");
        fs::create_dir_all(&admin).expect("test operation should succeed");
        fs::write(admin.join("commondir"), "../..\n").expect("test operation should succeed");

        let common = fs::canonicalize(common).expect("test operation should succeed");
        assert_eq!(repository_common_dir(&admin), common);
        assert_eq!(repository_objects_dir(&admin), common.join("objects"));

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn reachable_object_helpers_walk_graph_and_install_pack() {
        let root = temp_root("sley-reachable-pack");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(destination_git_dir.join("objects"))
            .expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"reachable payload\n".to_vec());
        let blob_oid = source
            .write_object(blob.clone())
            .expect("test operation should succeed");
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: BString::from(b"payload.txt"),
                    oid: blob_oid,
                }],
            }
            .write(),
        );
        let tree_oid = source
            .write_object(tree.clone())
            .expect("test operation should succeed");
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid,
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"initial\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = source
            .write_object(commit.clone())
            .expect("test operation should succeed");

        let reachable = collect_reachable_object_ids(&source, format, std::iter::once(commit_oid))
            .expect("test operation should succeed");
        assert!(reachable.contains(&commit_oid));
        assert!(reachable.contains(&tree_oid));
        assert!(reachable.contains(&blob_oid));

        let install =
            install_reachable_pack(&source, &destination, format, std::iter::once(commit_oid))
                .expect("test operation should succeed")
                .expect("reachable pack should be written");
        assert_eq!(install.object_ids.len(), 3);
        for (oid, object) in [
            (&commit_oid, &commit),
            (&tree_oid, &tree),
            (&blob_oid, &blob),
        ] {
            assert!(
                !destination
                    .loose()
                    .object_path(oid)
                    .expect("test operation should succeed")
                    .exists()
            );
            assert!(
                destination
                    .contains(oid)
                    .expect("test operation should succeed")
            );
            assert_eq!(read_object_for_assert(&destination, oid), *object);
        }
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn reachable_object_helpers_respect_exclusions_and_duplicate_starts() {
        let root = temp_root("sley-reachable-exclusions");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"excluded payload\n".to_vec());
        let blob_oid = db
            .write_object(blob)
            .expect("test operation should succeed");
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: BString::from(b"payload.txt"),
                    oid: blob_oid,
                }],
            }
            .write(),
        );
        let tree_oid = db
            .write_object(tree)
            .expect("test operation should succeed");
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid,
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"initial\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = db
            .write_object(commit)
            .expect("test operation should succeed");
        let excluded = HashSet::from([tree_oid]);

        let objects = collect_reachable_objects(&db, format, [commit_oid, commit_oid], &excluded)
            .expect("test operation should succeed");

        assert_eq!(objects.len(), 1);
        assert_eq!(
            objects[0]
                .object_id(format)
                .expect("test operation should succeed"),
            commit_oid
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn build_reachable_pack_returns_raw_pack_and_respects_empty_exclusions() {
        let root = temp_root("sley-build-reachable-pack");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let object = EncodedObject::new(ObjectType::Blob, b"raw reachable pack\n".to_vec());
        let oid = db
            .write_object(object.clone())
            .expect("test operation should succeed");
        let pack = build_reachable_pack(&db, format, std::iter::once(oid), &HashSet::new())
            .expect("test operation should succeed")
            .expect("reachable pack should be built");
        assert!(pack.pack.starts_with(b"PACK"));
        assert_eq!(pack.entries.len(), 1);
        assert_eq!(pack.entries[0].oid, oid);

        let excluded = HashSet::from([oid]);
        assert!(
            build_reachable_pack(
                &db,
                format,
                pack.entries.into_iter().map(|entry| entry.oid),
                &excluded
            )
            .expect("test operation should succeed")
            .is_none()
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn reachable_object_helpers_follow_tags_and_report_missing_objects() {
        let root = temp_root("sley-reachable-tags");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"tagged payload\n".to_vec());
        let blob_oid = db
            .write_object(blob)
            .expect("test operation should succeed");
        let tag = EncodedObject::new(
            ObjectType::Tag,
            Tag {
                object: blob_oid,
                object_type: ObjectType::Blob,
                name: b"v1".to_vec(),
                tagger: Some(b"Example <example@example.invalid> 0 +0000".to_vec()),
                message: b"tag message\n".to_vec(),
            }
            .write(),
        );
        let tag_oid = db.write_object(tag).expect("test operation should succeed");

        let reachable = collect_reachable_object_ids(&db, format, std::iter::once(tag_oid))
            .expect("test operation should succeed");
        assert!(reachable.contains(&tag_oid));
        assert!(reachable.contains(&blob_oid));

        let missing = ObjectId::from_hex(format, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("test operation should succeed");
        assert!(matches!(
            collect_reachable_object_ids(&db, format, std::iter::once(missing)),
            Err(GitError::NotFound(_))
        ));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn install_reachable_pack_empty_starts_create_no_pack() {
        let root = temp_root("sley-reachable-empty");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(destination_git_dir.join("objects"))
            .expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);

        let result = install_reachable_pack(&source, &destination, format, Vec::<ObjectId>::new())
            .expect("test operation should succeed");

        assert!(result.is_none());
        assert!(!destination_git_dir.join("objects").join("pack").exists());
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn install_reachable_pack_excluding_skips_fully_excluded_starts() {
        let root = temp_root("sley-reachable-install-excluding");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(destination_git_dir.join("objects"))
            .expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);
        let object = EncodedObject::new(ObjectType::Blob, b"excluded install\n".to_vec());
        let oid = source
            .write_object(object)
            .expect("test operation should succeed");
        let excluded = HashSet::from([oid]);

        let result = install_reachable_pack_excluding(
            &source,
            &destination,
            format,
            std::iter::once(oid),
            &excluded,
        )
        .expect("test operation should succeed");

        assert!(result.is_none());
        assert!(!destination_git_dir.join("objects").join("pack").exists());
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn install_reachable_pack_supports_sha256() {
        let root = temp_root("sley-reachable-pack-sha256");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(destination_git_dir.join("objects"))
            .expect("test operation should succeed");
        let format = ObjectFormat::Sha256;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);
        let object = EncodedObject::new(ObjectType::Blob, b"sha256 reachable pack\n".to_vec());
        let oid = source
            .write_object(object.clone())
            .expect("test operation should succeed");

        let pack = build_reachable_pack(&source, format, std::iter::once(oid), &HashSet::new())
            .expect("test operation should succeed")
            .expect("sha256 reachable pack should be built");
        assert!(pack.pack.starts_with(b"PACK"));
        assert_eq!(pack.entries[0].oid, oid);

        let result = install_reachable_pack(&source, &destination, format, std::iter::once(oid))
            .expect("test operation should succeed")
            .expect("sha256 reachable pack should be written");

        assert_eq!(result.object_ids, vec![oid]);
        assert!(
            !destination
                .loose()
                .object_path(&oid)
                .expect("test operation should succeed")
                .exists()
        );
        assert_eq!(read_object_for_assert(&destination, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn install_helpers_accept_custom_raw_pack_installer() {
        #[derive(Default)]
        struct RecordingInstaller {
            packs: std::cell::RefCell<Vec<Vec<u8>>>,
            installed: std::cell::RefCell<Vec<ObjectId>>,
        }

        impl RawPackInstaller for RecordingInstaller {
            fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult> {
                self.packs.borrow_mut().push(pack_bytes.to_vec());
                let object_ids = self.installed.borrow().clone();
                Ok(RawPackInstallResult { object_ids })
            }
        }

        let format = ObjectFormat::Sha1;
        let mut source = ObjectDatabase::new(format);
        let object = EncodedObject::new(ObjectType::Blob, b"custom raw installer\n".to_vec());
        let oid = source
            .write_object(object)
            .expect("test operation should succeed");
        let installer = RecordingInstaller::default();
        installer.installed.borrow_mut().push(oid);

        let result = install_reachable_pack(&source, &installer, format, std::iter::once(oid))
            .expect("test operation should succeed")
            .expect("custom installer should receive pack");

        assert_eq!(result.object_ids, installer.installed.into_inner());
        let packs = installer.packs.into_inner();
        assert_eq!(packs.len(), 1);
        assert!(packs[0].starts_with(b"PACK"));
    }

    #[test]
    fn file_database_reads_object_from_multi_pack_index() {
        let root = temp_root("sley-file-odb-midx");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("test operation should succeed");
        let first = EncodedObject::new(ObjectType::Blob, b"first packed\n".to_vec());
        let second = EncodedObject::new(ObjectType::Blob, b"second packed\n".to_vec());
        let first_oid = first
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let second_oid = second
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first))
            .expect("test operation should succeed");
        let second_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&second))
            .expect("test operation should succeed");
        let first_pack_name = format!("pack-{}.idx", first_pack.checksum.to_hex());
        let second_pack_name = format!("pack-{}.idx", second_pack.checksum.to_hex());
        fs::write(
            pack_dir.join(first_pack_name.replace(".idx", ".pack")),
            first_pack.pack,
        )
        .expect("test operation should succeed");
        fs::write(
            pack_dir.join(second_pack_name.replace(".idx", ".pack")),
            second_pack.pack,
        )
        .expect("test operation should succeed");
        let midx = MultiPackIndex::write(
            ObjectFormat::Sha1,
            2,
            &[first_pack_name, second_pack_name],
            &[
                sley_pack::MultiPackIndexEntry {
                    oid: first_oid,
                    pack_int_id: 0,
                    offset: first_pack.entries[0].offset,
                },
                sley_pack::MultiPackIndexEntry {
                    oid: second_oid,
                    pack_int_id: 1,
                    offset: second_pack.entries[0].offset,
                },
            ],
        )
        .expect("test operation should succeed");
        fs::write(pack_dir.join("multi-pack-index"), midx).expect("test operation should succeed");

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        assert!(
            db.contains(&second_oid)
                .expect("test operation should succeed")
        );
        assert_eq!(
            db.resolve_prefix(&second_oid.to_hex()[..8])
                .expect("test operation should succeed"),
            ObjectPrefixResolution::Unique(second_oid)
        );
        assert_eq!(read_object_for_assert(&db, &second_oid), second);
        assert_eq!(read_object_for_assert(&db, &first_oid), first);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_finds_pack_added_after_listing_was_cached() {
        // Regression guard for the cached pack-directory listing: a pack written
        // after the listing was first cached (via a prior read) must still be
        // discovered by the same handle, because a miss triggers a re-scan.
        let root = temp_root("sley-file-odb-pack-added-late");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        // First pack + object; reading it populates the listing cache.
        let first = EncodedObject::new(ObjectType::Blob, b"first late\n".to_vec());
        let first_oid = first
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first))
            .expect("test operation should succeed");
        db.install_pack(&first_pack)
            .expect("test operation should succeed");
        assert_eq!(read_object_for_assert(&db, &first_oid), first);

        // A second object that the cached listing does not yet know about.
        let second = EncodedObject::new(ObjectType::Blob, b"second late\n".to_vec());
        let second_oid = second
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        // It is genuinely absent right now.
        assert!(matches!(
            db.read_object(&second_oid),
            Err(GitError::NotFound(_))
        ));

        // Install its pack through the same handle; the next read must find it via
        // a re-scan, not be masked by the stale listing.
        let second_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&second))
            .expect("test operation should succeed");
        db.install_pack(&second_pack)
            .expect("test operation should succeed");
        assert!(
            db.contains(&second_oid)
                .expect("test operation should succeed")
        );
        assert_eq!(read_object_for_assert(&db, &second_oid), second);
        // The original object still resolves too.
        assert_eq!(read_object_for_assert(&db, &first_oid), first);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_prefers_loose_object_over_packed_object() {
        let root = temp_root("sley-file-odb-prefer-loose");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"same\n".to_vec());
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .expect("test operation should succeed");
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .expect("test operation should succeed");

        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(object.clone())
            .expect("test operation should succeed");
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn bundle_prerequisite_verification_reads_existing_objects() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"base\n".to_vec()))
            .expect("test operation should succeed");
        let bundle_bytes = format!("# v2 git bundle\n-{oid} base\n\n").into_bytes();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        verify_bundle_prerequisites(&bundle, &db).expect("test operation should succeed");
    }

    #[test]
    fn bundle_prerequisite_verification_reports_missing_objects() {
        let db = ObjectDatabase::new(ObjectFormat::Sha1);
        let missing = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"missing\n")
            .expect("test operation should succeed");
        let bundle_bytes = format!("# v2 git bundle\n-{missing} missing\n\n").into_bytes();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        assert!(verify_bundle_prerequisites(&bundle, &db).is_err());
    }

    #[test]
    fn unbundle_objects_writes_pack_entries_and_returns_refs() {
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"bundle object\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        let bundle_bytes = format!("# v2 git bundle\n{oid} refs/heads/main\n\n")
            .into_bytes()
            .into_iter()
            .chain(pack.pack)
            .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        let result = unbundle_objects(&bundle, &prerequisite_reader, &mut writer)
            .expect("test operation should succeed");
        assert_eq!(result.written_objects, vec![oid]);
        assert_eq!(result.references, bundle.references);
        assert_eq!(read_object_for_assert(&writer, &oid), object);
    }

    #[test]
    fn install_bundle_pack_writes_pack_and_returns_refs() {
        let root = temp_root("sley-install-bundle-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let database = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"bundle pack object\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        let bundle_bytes = format!("# v2 git bundle\n{oid} refs/heads/main\n\n")
            .into_bytes()
            .into_iter()
            .chain(pack.pack)
            .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        let result = install_bundle_pack(&bundle, &prerequisite_reader, &database)
            .expect("test operation should succeed");

        assert_eq!(result.written_objects, vec![oid]);
        assert_eq!(result.references, bundle.references);
        assert!(
            database
                .contains(&oid)
                .expect("test operation should succeed")
        );
        assert_eq!(read_object_for_assert(&database, &oid), object);
        assert!(
            !database
                .loose()
                .object_path(&oid)
                .expect("test operation should succeed")
                .exists()
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn unpack_packfile_objects_writes_sha256_pack_entries() {
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha256);
        let object = EncodedObject::new(ObjectType::Blob, b"transport pack object\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .expect("test operation should succeed");

        let result = unpack_packfile_objects(&pack.pack, ObjectFormat::Sha256, &mut writer)
            .expect("test operation should succeed");

        assert_eq!(result.written_objects, vec![oid]);
        assert_eq!(read_object_for_assert(&writer, &oid), object);
    }

    #[test]
    fn unbundle_objects_rejects_missing_prerequisites_before_writing() {
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha1);
        let missing = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"missing\n")
            .expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"bundle object\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        let bundle_bytes =
            format!("# v2 git bundle\n-{missing} missing\n{oid} refs/heads/main\n\n")
                .into_bytes()
                .into_iter()
                .chain(pack.pack)
                .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        assert!(unbundle_objects(&bundle, &prerequisite_reader, &mut writer).is_err());
        assert!(!writer.contains(&oid));
    }

    /// Build a commit -> tree -> blob graph in `db`, returning the three object
    /// ids and their canonical encodings as `(oid, object)` pairs.
    fn write_commit_graph(
        db: &mut FileObjectDatabase,
        payload: &[u8],
    ) -> Vec<(ObjectId, EncodedObject)> {
        let blob = EncodedObject::new(ObjectType::Blob, payload.to_vec());
        let blob_oid = db
            .write_object(blob.clone())
            .expect("test operation should succeed");
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: BString::from(b"payload.txt"),
                    oid: blob_oid,
                }],
            }
            .write(),
        );
        let tree_oid = db
            .write_object(tree.clone())
            .expect("test operation should succeed");
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid,
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"initial\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = db
            .write_object(commit.clone())
            .expect("test operation should succeed");
        vec![(commit_oid, commit), (tree_oid, tree), (blob_oid, blob)]
    }

    fn repack_all_objects_consolidates_loose_and_pack(format: ObjectFormat) {
        let root = temp_root("sley-repack-all");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        // A pre-existing pack holds one blob; the rest of the graph is loose.
        let packed_blob = EncodedObject::new(ObjectType::Blob, b"already packed\n".to_vec());
        let packed_oid = packed_blob
            .object_id(format)
            .expect("test operation should succeed");
        let existing_pack = PackFile::write_undeltified(std::slice::from_ref(&packed_blob), format)
            .expect("test operation should succeed");
        let existing = db
            .install_pack(&existing_pack)
            .expect("test operation should succeed");

        let graph = write_commit_graph(&mut db, b"repack payload\n");

        let mut expected: HashMap<ObjectId, EncodedObject> = graph.iter().cloned().collect();
        expected.insert(packed_oid, packed_blob.clone());

        let result = repack_all_objects(&git_dir, format)
            .expect("test operation should succeed")
            .expect("repository has objects");

        // The new pack round-trips and contains every original object byte-for-byte.
        assert_eq!(result.object_count, expected.len());
        let parsed = PackFile::parse(&result.pack, format).expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), expected.len());
        for entry in &parsed.entries {
            let want = expected
                .get(&entry.entry.oid)
                .expect("packed object was in the repository");
            assert_eq!(&entry.object, want);
            assert_eq!(
                entry
                    .object
                    .object_id(format)
                    .expect("test operation should succeed"),
                entry.entry.oid
            );
        }
        // The generated index parses and agrees with the pack checksum.
        let idx = PackIndex::parse(&result.idx, format).expect("test operation should succeed");
        assert_eq!(idx.pack_checksum, parsed.checksum);
        assert_eq!(idx.entries.len(), expected.len());

        // The pre-existing pack is reported obsolete (by its .pack path).
        assert_eq!(result.obsolete_packs, vec![existing.pack_path.clone()]);
        // Every loose object id is reported as now packed.
        let mut want_loose: Vec<ObjectId> = graph.iter().map(|(oid, _)| *oid).collect();
        want_loose.sort_by_key(ObjectId::to_hex);
        assert_eq!(result.packed_loose, want_loose);
        assert!(!result.packed_loose.contains(&packed_oid));

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn repack_all_objects_consolidates_loose_and_pack_sha1() {
        repack_all_objects_consolidates_loose_and_pack(ObjectFormat::Sha1);
    }

    #[test]
    fn repack_all_objects_consolidates_loose_and_pack_sha256() {
        repack_all_objects_consolidates_loose_and_pack(ObjectFormat::Sha256);
    }

    #[test]
    fn repack_all_objects_returns_none_for_empty_repository() {
        let root = temp_root("sley-repack-empty");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");

        assert!(
            repack_all_objects(&git_dir, ObjectFormat::Sha1)
                .expect("test operation should succeed")
                .is_none()
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn install_repack_result_writes_pack_without_pruning_by_default() {
        let root = temp_root("sley-repack-install-nodelete");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, b"install no prune\n");

        let result = repack_all_objects(&git_dir, format)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        install_repack_result(&git_dir, format, &result, false)
            .expect("test operation should succeed");

        // New pack is on disk and readable.
        let parsed = PackFile::parse(&result.pack, format).expect("test operation should succeed");
        let pack_dir = git_dir.join("objects").join("pack");
        let pack_path = pack_dir.join(format!("pack-{}.pack", parsed.checksum.to_hex()));
        let idx_path = pack_dir.join(format!("pack-{}.idx", parsed.checksum.to_hex()));
        assert!(pack_path.exists());
        assert!(idx_path.exists());
        // Loose objects survive because prune was not requested.
        for (oid, object) in &graph {
            assert!(
                db.loose()
                    .object_path(oid)
                    .expect("test operation should succeed")
                    .exists()
            );
            assert_eq!(read_object_for_assert(&db, oid), *object);
        }

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn install_repack_result_prunes_obsolete_packs_and_loose_objects() {
        let root = temp_root("sley-repack-install-prune");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let packed_blob = EncodedObject::new(ObjectType::Blob, b"prune packed\n".to_vec());
        let existing_pack = PackFile::write_undeltified(std::slice::from_ref(&packed_blob), format)
            .expect("test operation should succeed");
        let existing = db
            .install_pack(&existing_pack)
            .expect("test operation should succeed");
        let graph = write_commit_graph(&mut db, b"prune payload\n");

        let result = repack_all_objects(&git_dir, format)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        let new_pack_checksum = PackFile::parse(&result.pack, format)
            .expect("test operation should succeed")
            .checksum;
        install_repack_result(&git_dir, format, &result, true)
            .expect("test operation should succeed");

        // Obsolete pack and its index are gone.
        assert!(!existing.pack_path.exists());
        assert!(!existing.index_path.exists());
        // Packed loose objects are gone from disk.
        for (oid, _) in &graph {
            assert!(
                !db.loose()
                    .object_path(oid)
                    .expect("test operation should succeed")
                    .exists()
            );
        }
        // The new consolidated pack remains and still serves every object.
        let pack_dir = git_dir.join("objects").join("pack");
        assert!(
            pack_dir
                .join(format!("pack-{}.pack", new_pack_checksum.to_hex()))
                .exists()
        );
        let reopened = FileObjectDatabase::from_git_dir(&git_dir, format);
        for (oid, object) in &graph {
            assert!(
                reopened
                    .contains(oid)
                    .expect("test operation should succeed")
            );
            assert_eq!(read_object_for_assert(&reopened, oid), *object);
        }
        let packed_oid = packed_blob
            .object_id(format)
            .expect("test operation should succeed");
        assert_eq!(read_object_for_assert(&reopened, &packed_oid), packed_blob);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn install_repack_result_preserves_keep_and_promisor_packs() {
        let root = temp_root("sley-repack-install-keep-promisor");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let keep_blob = EncodedObject::new(ObjectType::Blob, b"keep protected\n".to_vec());
        let keep_pack = PackFile::write_undeltified(std::slice::from_ref(&keep_blob), format)
            .expect("test operation should succeed");
        let keep_install = db
            .install_pack(&keep_pack)
            .expect("test operation should succeed");
        let keep_sidecar = keep_install.pack_path.with_extension("keep");
        fs::write(&keep_sidecar, b"").expect("test operation should succeed");

        let promisor_blob = EncodedObject::new(ObjectType::Blob, b"promisor protected\n".to_vec());
        let promisor_pack =
            PackFile::write_undeltified(std::slice::from_ref(&promisor_blob), format)
                .expect("test operation should succeed");
        let promisor_install = db
            .install_pack_with_options(&promisor_pack, RawPackInstallOptions { promisor: true })
            .expect("test operation should succeed");
        let promisor_sidecar = promisor_install
            .promisor_path
            .clone()
            .expect("promisor sidecar");

        let graph = write_commit_graph(&mut db, b"new consolidated payload\n");
        let result = repack_all_objects(&git_dir, format)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert!(result.obsolete_packs.contains(&keep_install.pack_path));
        assert!(result.obsolete_packs.contains(&promisor_install.pack_path));

        install_repack_result(&git_dir, format, &result, true)
            .expect("test operation should succeed");

        for path in [
            &keep_install.pack_path,
            &keep_install.index_path,
            &keep_sidecar,
            &promisor_install.pack_path,
            &promisor_install.index_path,
            &promisor_sidecar,
        ] {
            assert!(path.exists(), "{} should be preserved", path.display());
        }
        for (oid, _) in &graph {
            assert!(
                !db.loose()
                    .object_path(oid)
                    .expect("test operation should succeed")
                    .exists()
            );
        }

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn install_repack_result_keeps_loose_object_absent_from_new_pack() {
        // Safety: a loose object whose id is not in the new pack must survive
        // pruning even if the caller lists it in `packed_loose`.
        let root = temp_root("sley-repack-install-safety");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, b"safety packed\n");

        let mut result = repack_all_objects(&git_dir, format)
            .expect("test operation should succeed")
            .expect("test operation should succeed");

        // A loose object that is NOT in the new pack, but mislabeled as packed.
        let stray = EncodedObject::new(ObjectType::Blob, b"never packed\n".to_vec());
        let stray_oid = db
            .write_object(stray.clone())
            .expect("test operation should succeed");
        assert!(!result.packed_loose.contains(&stray_oid));
        result.packed_loose.push(stray_oid);

        install_repack_result(&git_dir, format, &result, true)
            .expect("test operation should succeed");

        // The stray loose object is untouched because it is not in the new pack.
        assert!(
            db.loose()
                .object_path(&stray_oid)
                .expect("test operation should succeed")
                .exists()
        );
        assert_eq!(read_object_for_assert(&db, &stray_oid), stray);
        // Genuinely packed loose objects were still removed.
        for (oid, _) in &graph {
            assert!(
                !db.loose()
                    .object_path(oid)
                    .expect("test operation should succeed")
                    .exists()
            );
        }

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn prune_unreachable_loose_reports_and_deletes_only_unreachable() {
        let root = temp_root("sley-prune-unreachable");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, b"reachable payload\n");
        let commit_oid = graph[0].0.clone();

        // A dangling loose blob not referenced by the commit graph.
        let dangling = EncodedObject::new(ObjectType::Blob, b"dangling\n".to_vec());
        let dangling_oid = db
            .write_object(dangling)
            .expect("test operation should succeed");

        // Report-only pass leaves everything on disk.
        let reported = prune_unreachable_loose(&git_dir, format, [commit_oid], false)
            .expect("test operation should succeed");
        assert_eq!(reported, vec![dangling_oid]);
        assert!(
            db.loose()
                .object_path(&dangling_oid)
                .expect("test operation should succeed")
                .exists()
        );

        // Deleting pass removes only the unreachable object.
        let deleted = prune_unreachable_loose(&git_dir, format, [commit_oid], true)
            .expect("test operation should succeed");
        assert_eq!(deleted, vec![dangling_oid]);
        assert!(
            !db.loose()
                .object_path(&dangling_oid)
                .expect("test operation should succeed")
                .exists()
        );
        for (oid, object) in &graph {
            assert!(
                db.loose()
                    .object_path(oid)
                    .expect("test operation should succeed")
                    .exists()
            );
            assert_eq!(read_object_for_assert(&db, oid), *object);
        }

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn prune_unreachable_loose_ignores_gitlink_targets() {
        let root = temp_root("sley-prune-gitlink");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let submodule_oid = ObjectId::from_hex(format, "1111111111111111111111111111111111111111")
            .expect("test operation should succeed");
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o160000,
                    name: BString::from(b"submodule"),
                    oid: submodule_oid,
                }],
            }
            .write(),
        );
        let tree_oid = db
            .write_object(tree)
            .expect("test operation should succeed");
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid,
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"gitlink\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = db
            .write_object(commit)
            .expect("test operation should succeed");
        let dangling = EncodedObject::new(ObjectType::Blob, b"dangling with gitlink\n".to_vec());
        let dangling_oid = db
            .write_object(dangling)
            .expect("test operation should succeed");

        let deleted = prune_unreachable_loose(&git_dir, format, [commit_oid], true)
            .expect("test operation should succeed");

        assert_eq!(deleted, vec![dangling_oid]);
        assert!(
            !db.loose()
                .object_path(&dangling_oid)
                .expect("test operation should succeed")
                .exists()
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn temp_root(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
