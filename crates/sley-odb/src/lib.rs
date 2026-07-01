// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::{Decompress, FlushDecompress};
use sley_core::{GitError, MissingObjectContext, ObjectFormat, ObjectId, Result};
use sley_formats::{Bundle, BundleReference};
use sley_object::{
    Commit, EncodedObject, ObjectType, Tag, TreeEntries, parse_framed_object,
    tree_entry_object_type,
};
use sley_pack::{
    MultiPackIndex, MultiPackIndexOidLookup, PackBitmapIndex, PackBitmapWriter, PackFile,
    PackIndex, PackIndexByteSource, PackIndexEntry, PackIndexViewData, PackInput,
    PackStreamIndexBuild, PackWrite, PackWriteOptions, PackWriteSummary,
};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::{env, fs};

static TEMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub trait ObjectReader {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>>;

    /// Graft-points seam (shallow clones today, replace refs/grafts later):
    /// `true` when history is cut at `oid`, so every walk must treat the
    /// commit as parentless even though its raw body still names parents.
    ///
    /// [`FileObjectDatabase`] answers from `$GIT_DIR/shallow`; readers that
    /// are not backed by a repository (in-memory stores, pack overlays)
    /// keep the default "no grafts".
    fn is_shallow_graft(&self, _oid: &ObjectId) -> bool {
        false
    }

    /// Whether this reader has any shallow/graft boundaries at all. Walkers can
    /// use this to choose dense graph-only traversal when no boundary can cut
    /// parent edges.
    fn has_shallow_grafts(&self) -> bool {
        false
    }

    /// True when `oid` is covered by a promisor pack. Partial clones are
    /// allowed to omit promised objects until a later on-demand fetch hydrates
    /// them; ordinary readers keep the default "no promised objects".
    fn is_promised_object(&self, _oid: &ObjectId) -> bool {
        false
    }
}

fn implied_empty_tree_object(format: ObjectFormat, oid: &ObjectId) -> Option<Arc<EncodedObject>> {
    (*oid == ObjectId::empty_tree(format))
        .then(|| Arc::new(EncodedObject::new(ObjectType::Tree, Vec::new())))
}

fn with_missing_object_context(
    err: GitError,
    oid: ObjectId,
    context: MissingObjectContext,
) -> GitError {
    let kind = err
        .not_found_kind()
        .and_then(sley_core::NotFoundKind::missing_object_kind);
    match kind {
        Some(kind) => GitError::object_kind_not_found_in(oid, kind, context),
        None => err,
    }
}

/// Parents of a parsed commit with the graft seam applied: empty when the
/// reader cuts history at `oid` (shallow boundary), the raw parsed parents
/// otherwise.
pub fn grafted_parents<R: ObjectReader + ?Sized>(
    reader: &R,
    oid: &ObjectId,
    parents: Vec<ObjectId>,
) -> Vec<ObjectId> {
    if reader.is_shallow_graft(oid) {
        Vec::new()
    } else {
        parents
    }
}

pub trait ObjectWriter {
    /// Write `object`, returning its id. Takes `&self`: every implementation's
    /// write state (in-memory map, loose-object cache) is behind interior
    /// mutability, so a single handle can interleave reads and writes without a
    /// `&mut` borrow. This lets the merge engine read and write through one `db`
    /// instead of opening a second read-only handle that re-warms the caches.
    fn write_object(&self, object: EncodedObject) -> Result<ObjectId>;
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

#[derive(Debug)]
pub struct RawPackStreamingInstall {
    format: ObjectFormat,
    expected_pack_id: ObjectId,
    expected_pack_size: u64,
    options: RawPackInstallOptions,
    pack_dir: PathBuf,
    pack_name: String,
    pack_path: PathBuf,
    index_path: PathBuf,
    temp_pack_path: PathBuf,
    file: Option<fs::File>,
    written: u64,
    finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPackInstallResult {
    pub object_ids: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPackIndexResult {
    pub pack_id: ObjectId,
    pub index: Vec<u8>,
    pub objects: Vec<RawPackIndexedObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPackIndexedObject {
    pub oid: ObjectId,
    pub object_type: ObjectType,
    pub size: u64,
    pub offset: u64,
}

struct PackInstallTeeReader<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
}

impl<R, W> Read for PackInstallTeeReader<'_, R, W>
where
    R: Read,
    W: Write,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = self.reader.read(buf)?;
        if len > 0 {
            self.writer.write_all(&buf[..len])?;
        }
        Ok(len)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachablePackFile {
    pub pack_path: PathBuf,
    pub pack_size: u64,
    pub checksum: ObjectId,
    pub object_count: usize,
    pub delta_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachablePackWriteSummary {
    pub index: Vec<u8>,
    pub checksum: ObjectId,
    pub object_count: usize,
    pub delta_count: u32,
    pub pack_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawPackInstallOptions {
    pub promisor: bool,
}

pub trait RawPackInstaller {
    fn install_raw_pack_from_reader<R>(&self, reader: &mut R) -> Result<RawPackInstallResult>
    where
        R: Read;
}

#[cfg(test)]
const REACHABLE_PACK_STREAMING_MIN_OBJECTS: usize = 32;
#[cfg(not(test))]
const REACHABLE_PACK_STREAMING_MIN_OBJECTS: usize = 4096;

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
    fn install_raw_pack_from_reader<R>(&self, reader: &mut R) -> Result<RawPackInstallResult>
    where
        R: Read,
    {
        let result = FileObjectDatabase::install_raw_pack_from_reader(self, reader)?;
        Ok(RawPackInstallResult {
            object_ids: result.object_ids,
        })
    }
}

impl RawPackInstaller for ObjectDatabase {
    fn install_raw_pack_from_reader<R>(&self, reader: &mut R) -> Result<RawPackInstallResult>
    where
        R: Read,
    {
        let mut pack_bytes = Vec::new();
        reader.read_to_end(&mut pack_bytes)?;
        let result = unpack_packfile_objects(&pack_bytes, self.format, self)?;
        Ok(RawPackInstallResult {
            object_ids: result.written_objects,
        })
    }
}

impl RawPackStreamingInstall {
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    pub fn pack_path(&self) -> &Path {
        &self.pack_path
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    pub fn finish(mut self) -> Result<PackInstallResult> {
        let result = (|| -> Result<PackInstallResult> {
            let mut file = self.file.take().ok_or_else(|| {
                GitError::InvalidFormat("raw pack stream already finished".into())
            })?;
            file.flush()?;
            file.sync_all()?;
            drop(file);

            if self.written != self.expected_pack_size {
                return Err(GitError::InvalidFormat(format!(
                    "raw pack stream length mismatch: expected {}, got {}",
                    self.expected_pack_size, self.written
                )));
            }

            let built = PackIndex::write_v2_for_pack_path(&self.temp_pack_path, self.format)?;
            if built.pack_checksum != self.expected_pack_id {
                return Err(GitError::InvalidFormat(format!(
                    "raw pack stream checksum mismatch: expected {}, got {}",
                    self.expected_pack_id, built.pack_checksum
                )));
            }

            match fs::rename(&self.temp_pack_path, &self.pack_path) {
                Ok(()) => {}
                Err(_) if self.pack_path.exists() => {
                    let _ = fs::remove_file(&self.temp_pack_path);
                }
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
            write_pack_component(&self.index_path, &built.index)?;
            let promisor_path = write_promisor_pack_sidecar(
                &self.pack_dir,
                &self.pack_name,
                self.options.promisor,
            )?;
            Ok(PackInstallResult {
                pack_name: self.pack_name.clone(),
                pack_path: self.pack_path.clone(),
                index_path: self.index_path.clone(),
                promisor_path,
                object_ids: built.entries.iter().map(|entry| entry.oid).collect(),
            })
        })();

        if result.is_ok() {
            self.finished = true;
        } else {
            let _ = fs::remove_file(&self.temp_pack_path);
        }
        result
    }
}

impl Write for RawPackStreamingInstall {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let next_written = self.written.checked_add(buf.len() as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "pack size overflow")
        })?;
        if next_written > self.expected_pack_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "raw pack stream exceeds expected size {}; got at least {}",
                    self.expected_pack_size, next_written
                ),
            ));
        }
        let file = self.file.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "raw pack stream already finished",
            )
        })?;
        let written = file.write(buf)?;
        self.written = self.written.checked_add(written as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "pack size overflow")
        })?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for RawPackStreamingInstall {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.file.take();
            let _ = fs::remove_file(&self.temp_pack_path);
        }
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
    Err(GitError::object_not_found_in(
        missing[0],
        MissingObjectContext::PackInstall,
    ))
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
    let mut reader = bundle.pack.as_slice();
    let install = destination.install_raw_pack_from_reader(&mut reader)?;
    Ok(BundleUnbundleResult {
        written_objects: install.object_ids,
        references: bundle.references.clone(),
    })
}

pub fn unpack_packfile_objects<W>(
    pack_bytes: &[u8],
    format: ObjectFormat,
    writer: &W,
) -> Result<PackUnpackResult>
where
    W: ObjectWriter,
{
    let pack = PackFile::parse(pack_bytes, format)?;
    write_pack_objects(pack, writer, "pack")
}

pub fn index_raw_pack(pack_bytes: &[u8], format: ObjectFormat) -> Result<RawPackIndexResult> {
    let pack = PackFile::parse(pack_bytes, format)?;
    let built = PackIndex::write_v2_for_pack(pack_bytes, format)?;
    if built.pack_checksum != pack.checksum {
        return Err(GitError::InvalidFormat(
            "pack index checksum does not match parsed pack checksum".to_string(),
        ));
    }

    let offsets = built
        .entries
        .iter()
        .map(|entry| (entry.oid, entry.offset))
        .collect::<HashMap<_, _>>();
    let mut objects = Vec::with_capacity(pack.entries.len());
    for object in pack.entries {
        let offset = offsets.get(&object.entry.oid).copied().ok_or_else(|| {
            GitError::InvalidFormat(format!(
                "pack index is missing object {}",
                object.entry.oid.to_hex()
            ))
        })?;
        objects.push(RawPackIndexedObject {
            oid: object.entry.oid,
            object_type: object.object.object_type,
            size: object.object.body.len() as u64,
            offset,
        });
    }

    Ok(RawPackIndexResult {
        pack_id: built.pack_checksum,
        index: built.index,
        objects,
    })
}

pub fn index_raw_pack_from_reader<R>(
    reader: &mut R,
    format: ObjectFormat,
) -> Result<RawPackIndexResult>
where
    R: Read,
{
    Ok(stream_index_build_to_raw_result(
        PackIndex::write_v2_for_pack_reader_to_trailer(reader, format)?,
    ))
}

pub fn index_raw_pack_from_reader_with_len<R>(
    reader: &mut R,
    format: ObjectFormat,
    pack_len: u64,
) -> Result<RawPackIndexResult>
where
    R: Read,
{
    Ok(stream_index_build_to_raw_result(
        PackIndex::write_v2_for_pack_reader_with_len(reader, format, pack_len)?,
    ))
}

pub fn index_raw_pack_file(
    path: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<RawPackIndexResult> {
    Ok(stream_index_build_to_raw_result(
        PackIndex::write_v2_for_pack_path(path, format)?,
    ))
}

fn stream_index_build_to_raw_result(built: PackStreamIndexBuild) -> RawPackIndexResult {
    let objects = built
        .objects
        .into_iter()
        .map(|object| RawPackIndexedObject {
            oid: object.oid,
            object_type: object.object_type,
            size: object.size,
            offset: object.offset,
        })
        .collect::<Vec<_>>();
    RawPackIndexResult {
        pack_id: built.pack_checksum,
        index: built.index,
        objects,
    }
}

fn write_pack_objects<W>(pack: PackFile, writer: &W, source: &str) -> Result<PackUnpackResult>
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

fn collect_reachable_object_ids_tolerating_missing<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    walk_reachable_objects_tolerating_missing(reader, format, starts)
}

/// [`collect_reachable_object_ids`] with a cut set: commits in `cut` are
/// collected, but the walk does not continue to their parents — the view a
/// shallow repository has of its own refs (`$GIT_DIR/shallow` of the *other*
/// side, threaded explicitly because `reader` belongs to this side).
pub fn collect_reachable_object_ids_with_cut<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    cut: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    walk_reachable_objects_with_cut(reader, format, starts, &HashSet::new(), cut, |_, _| {})
}

/// [`collect_reachable_object_ids`] with a stop set: objects in `excluded` are
/// not visited and not expanded, so the walk never sees anything reachable only
/// through them (used to truncate history at a shallow boundary).
pub fn collect_reachable_object_ids_excluding<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    walk_reachable_objects(reader, format, starts, excluded, |_, _| {})
}

fn collect_reachable_object_ids_excluding_promised_missing<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let mut seen = HashSet::new();
    let mut pending: Vec<ObjectId> = starts.into_iter().collect();
    while let Some(oid) = pending.pop() {
        if excluded.contains(&oid) || !seen.insert(oid) {
            continue;
        }
        let object = match reader
            .read_object(&oid)
            .map_err(|err| with_missing_object_context(err, oid, MissingObjectContext::Traversal))
        {
            Ok(object) => object,
            Err(GitError::NotFound(_)) if reader.is_promised_object(&oid) => continue,
            Err(err) => return Err(err),
        };
        match object.object_type {
            ObjectType::Commit => {
                let commit = Commit::parse_ref(format, &object.body)?;
                pending.extend(grafted_parents(reader, &oid, commit.parents));
                pending.push(commit.tree);
            }
            ObjectType::Tree => {
                for entry in TreeEntries::new(format, &object.body) {
                    let entry = entry?;
                    if !entry.is_gitlink() {
                        pending.push(entry.oid);
                    }
                }
            }
            ObjectType::Tag => {
                let tag = Tag::parse_ref(format, &object.body)?;
                pending.push(tag.object);
            }
            ObjectType::Blob => {}
        }
    }
    Ok(seen)
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReachablePackObjectMeta {
    oid: ObjectId,
    object_type: ObjectType,
    size: u64,
}

enum ReachablePackObjectsForWrite {
    Buffered(Vec<ReachablePackObject>),
    Streaming(Vec<ReachablePackObjectMeta>),
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

fn collect_reachable_pack_objects_for_write<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<ReachablePackObjectsForWrite>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let mut buffered = Some(Vec::new());
    let mut metadata = Vec::new();
    walk_reachable_objects(reader, format, starts, excluded, |oid, object| {
        metadata.push(ReachablePackObjectMeta {
            oid: *oid,
            object_type: object.object_type,
            size: object.body.len() as u64,
        });
        let should_stream = buffered
            .as_ref()
            .is_some_and(|objects| objects.len() + 1 >= REACHABLE_PACK_STREAMING_MIN_OBJECTS);
        if should_stream {
            buffered = None;
        }
        if let Some(objects) = buffered.as_mut() {
            objects.push(ReachablePackObject {
                oid: *oid,
                object: Arc::clone(object),
            });
        }
    })?;

    match buffered {
        Some(objects) => Ok(ReachablePackObjectsForWrite::Buffered(objects)),
        None => {
            sort_reachable_pack_metadata(&mut metadata);
            Ok(ReachablePackObjectsForWrite::Streaming(metadata))
        }
    }
}

fn sort_reachable_pack_metadata(metadata: &mut [ReachablePackObjectMeta]) {
    metadata.sort_by(|left, right| {
        reachable_pack_type_rank(left.object_type)
            .cmp(&reachable_pack_type_rank(right.object_type))
            .then_with(|| right.size.cmp(&left.size))
            .then_with(|| left.oid.as_bytes().cmp(right.oid.as_bytes()))
    });
}

fn reachable_pack_type_rank(object_type: ObjectType) -> u8 {
    match object_type {
        ObjectType::Commit => 0,
        ObjectType::Tree => 1,
        ObjectType::Blob => 2,
        ObjectType::Tag => 3,
    }
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
    let mut reader = pack.pack.as_slice();
    destination
        .install_raw_pack_from_reader(&mut reader)
        .map(Some)
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

pub fn build_reachable_pack_file<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
    pack_path: impl AsRef<Path>,
) -> Result<Option<ReachablePackFile>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let objects = collect_reachable_pack_objects(reader, format, starts, excluded)?;
    if objects.is_empty() {
        return Ok(None);
    }
    let inputs = pack_inputs(&objects);
    let pack_path = pack_path.as_ref();
    if let Some(parent) = pack_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(pack_path)?;
    let summary = PackFile::write_packed_with_known_ids_to_writer(
        &inputs,
        format,
        &PackWriteOptions::new(),
        &mut file,
    )?;
    file.sync_all()?;
    Ok(Some(reachable_pack_file_result(pack_path, summary)))
}

pub fn write_reachable_pack_to_writer<R, I, W>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
    writer: &mut W,
) -> Result<Option<ReachablePackWriteSummary>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
    W: Write,
{
    match collect_reachable_pack_objects_for_write(reader, format, starts, excluded)? {
        ReachablePackObjectsForWrite::Buffered(objects) => {
            if objects.is_empty() {
                return Ok(None);
            }
            let inputs = pack_inputs(&objects);
            let summary = PackFile::write_packed_with_known_ids_to_writer(
                &inputs,
                format,
                &PackWriteOptions::new(),
                writer,
            )?;
            Ok(Some(reachable_pack_write_summary(summary)))
        }
        ReachablePackObjectsForWrite::Streaming(metadata) => {
            if metadata.is_empty() {
                return Ok(None);
            }
            let object_ids = metadata.iter().map(|meta| meta.oid).collect::<Vec<_>>();
            write_object_id_pack_to_writer(reader, format, &object_ids, writer).map(Some)
        }
    }
}

pub fn write_object_id_pack_to_writer<R, W>(
    reader: &R,
    format: ObjectFormat,
    object_ids: &[ObjectId],
    writer: &mut W,
) -> Result<ReachablePackWriteSummary>
where
    R: ObjectReader,
    W: Write,
{
    let summary = PackFile::write_packed_from_source_to_writer(
        object_ids,
        format,
        &PackWriteOptions::new(),
        |oid| reader.read_object(oid),
        writer,
    )?;
    Ok(reachable_pack_write_summary(summary))
}

fn reachable_pack_file_result(path: &Path, summary: PackWriteSummary) -> ReachablePackFile {
    ReachablePackFile {
        pack_path: path.to_path_buf(),
        pack_size: summary.pack_size,
        checksum: summary.checksum,
        object_count: summary.entries.len(),
        delta_count: summary.delta_count,
    }
}

fn reachable_pack_write_summary(summary: PackWriteSummary) -> ReachablePackWriteSummary {
    ReachablePackWriteSummary {
        index: summary.index,
        checksum: summary.checksum,
        object_count: summary.entries.len(),
        delta_count: summary.delta_count,
        pack_size: summary.pack_size,
    }
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
    build_and_install_reachable_pack_filtered(
        source,
        destination,
        format,
        starts,
        excluded,
        options,
        None,
        None,
    )
}

/// A partial-clone object filter applied while building a transfer pack.
///
/// Mirrors the subset of upstream's `list-objects-filter` the in-process local
/// server supports: directly-wanted tips are always packed; the filter only
/// prunes objects reached *through* the traversal (upstream's
/// `filter_blobs_none` runs on traversed blobs, never on wanted tips).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackObjectFilter {
    /// `blob:none`: omit every blob reached through tree traversal.
    BlobNone,
    /// `blob:limit=<n>`: omit traversed blobs whose body is at least `n` bytes.
    BlobLimit(u64),
    /// `tree:<n>`: keep only trees shallower than `n`, and omit traversed blobs.
    TreeDepth(u32),
    /// `sparse:oid=<blob>`: keep only blobs whose repo path is listed.
    SparsePathSet(Vec<String>),
}

/// [`build_and_install_reachable_pack`] with an optional partial-clone
/// `filter`. With `Some(BlobNone)`, blobs are dropped from the pack unless
/// they are directly wanted (named in `starts`).
#[allow(clippy::too_many_arguments)]
pub fn build_and_install_reachable_pack_filtered<R, I>(
    source: &R,
    destination: &FileObjectDatabase,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
    options: RawPackInstallOptions,
    filter: Option<PackObjectFilter>,
    unpack_limit: Option<usize>,
) -> Result<Option<PackInstallResult>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let starts: Vec<ObjectId> = starts.into_iter().collect();
    let wanted: HashSet<ObjectId> = starts.iter().copied().collect();
    let mut objects = collect_reachable_pack_objects(source, format, starts, excluded)?;
    match filter {
        Some(PackObjectFilter::BlobNone) => {
            objects.retain(|entry| {
                entry.object.object_type != ObjectType::Blob || wanted.contains(&entry.oid)
            });
        }
        Some(PackObjectFilter::BlobLimit(limit)) => {
            objects.retain(|entry| {
                entry.object.object_type != ObjectType::Blob
                    || wanted.contains(&entry.oid)
                    || (entry.object.body.len() as u64) < limit
            });
        }
        Some(PackObjectFilter::TreeDepth(depth)) => {
            let tree_depths = collect_tree_filter_depths(source, format, &objects)?;
            objects.retain(|entry| {
                if wanted.contains(&entry.oid) {
                    return true;
                }
                match entry.object.object_type {
                    ObjectType::Blob => false,
                    ObjectType::Tree => tree_depths
                        .get(&entry.oid)
                        .is_some_and(|tree_depth| *tree_depth < depth),
                    _ => true,
                }
            });
        }
        Some(PackObjectFilter::SparsePathSet(paths)) => {
            let allowed_blobs = collect_sparse_filter_blobs(source, format, &objects, &paths)?;
            objects.retain(|entry| {
                entry.object.object_type != ObjectType::Blob
                    || wanted.contains(&entry.oid)
                    || allowed_blobs.contains(&entry.oid)
            });
        }
        None => {}
    }
    if objects.is_empty() {
        return Ok(None);
    }
    // Mirror fetch-pack's unpack-limit: small transfers are exploded into
    // loose objects instead of landing as a pack (upstream `get_pack` picks
    // unpack-objects when the header count is below fetch/transfer.unpackLimit).
    if let Some(limit) = unpack_limit
        && objects.len() < limit
    {
        for entry in &objects {
            destination.loose().write_object((*entry.object).clone())?;
        }
        return Ok(None);
    }
    let inputs = pack_inputs(&objects);
    let pack_dir = destination.objects_dir.join("pack");
    fs::create_dir_all(&pack_dir)?;
    let temp_pack_path = unique_temp_path(&pack_dir);
    let result = (|| -> Result<PackInstallResult> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_pack_path)?;
        let summary = PackFile::write_packed_with_known_ids_to_writer(
            &inputs,
            format,
            &PackWriteOptions::new(),
            &mut file,
        )?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        trace_packfile_path(&temp_pack_path)?;
        destination.install_pack_file_from_temp(
            &temp_pack_path,
            summary.checksum,
            &summary.index,
            summary.entries.iter().map(|entry| entry.oid).collect(),
            options,
        )
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_pack_path);
    }
    result.map(Some)
}

fn trace_packfile_path(pack_path: &Path) -> Result<()> {
    let Some(path) = env::var_os("GIT_TRACE_PACKFILE").filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    fs::copy(pack_path, path)?;
    Ok(())
}

fn collect_tree_filter_depths<R>(
    reader: &R,
    format: ObjectFormat,
    objects: &[ReachablePackObject],
) -> Result<HashMap<ObjectId, u32>>
where
    R: ObjectReader,
{
    let available: HashSet<ObjectId> = objects.iter().map(|entry| entry.oid).collect();
    let mut depths = HashMap::new();
    let mut stack = Vec::new();
    for entry in objects {
        if entry.object.object_type != ObjectType::Commit {
            continue;
        }
        let commit = Commit::parse(format, &entry.object.body)?;
        if available.contains(&commit.tree) {
            stack.push((commit.tree, 0u32));
        }
    }
    while let Some((tree_oid, depth)) = stack.pop() {
        if depths
            .get(&tree_oid)
            .is_some_and(|old_depth| *old_depth <= depth)
        {
            continue;
        }
        depths.insert(tree_oid, depth);
        let tree = reader.read_object(&tree_oid)?;
        if tree.object_type != ObjectType::Tree {
            continue;
        }
        let child_depth = depth.saturating_add(1);
        for entry in TreeEntries::new(format, &tree.body) {
            let entry = entry?;
            if tree_entry_object_type(entry.mode) == ObjectType::Tree
                && available.contains(&entry.oid)
            {
                stack.push((entry.oid, child_depth));
            }
        }
    }
    Ok(depths)
}

fn collect_sparse_filter_blobs<R>(
    reader: &R,
    format: ObjectFormat,
    objects: &[ReachablePackObject],
    paths: &[String],
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
{
    let wanted_paths: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut allowed = HashSet::new();
    let mut seen_trees = HashSet::new();
    for entry in objects {
        if entry.object.object_type != ObjectType::Commit {
            continue;
        }
        let commit = Commit::parse(format, &entry.object.body)?;
        collect_sparse_tree_blobs(
            reader,
            format,
            &commit.tree,
            "",
            &wanted_paths,
            &mut seen_trees,
            &mut allowed,
        )?;
    }
    Ok(allowed)
}

fn collect_sparse_tree_blobs<R>(
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &str,
    wanted_paths: &HashSet<&str>,
    seen_trees: &mut HashSet<ObjectId>,
    allowed: &mut HashSet<ObjectId>,
) -> Result<()>
where
    R: ObjectReader,
{
    if !seen_trees.insert(*tree_oid) {
        return Ok(());
    }
    let tree = reader.read_object(tree_oid)?;
    if tree.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(format, &tree.body) {
        let entry = entry?;
        let name = String::from_utf8_lossy(entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if tree_entry_object_type(entry.mode) == ObjectType::Tree {
            collect_sparse_tree_blobs(
                reader,
                format,
                &entry.oid,
                &path,
                wanted_paths,
                seen_trees,
                allowed,
            )?;
        } else if wanted_paths.contains(path.as_str()) {
            allowed.insert(entry.oid);
        }
    }
    Ok(())
}

/// Assemble a pack stream that reuses an existing pack's object data verbatim
/// (upstream pack-objects' "pack reuse" fast path, full-pack case) and appends
/// `appended` as freshly encoded undeltified entries.
///
/// The reused pack's entry bytes are copied as-is between our own header and
/// trailer: a full-pack copy preserves every relative distance, so internal
/// `OFS_DELTA` bases stay valid. The header object count covers both the
/// reused and appended entries, and the trailing pack checksum is recomputed
/// over the assembled stream.
pub fn assemble_pack_with_verbatim_reuse(
    format: ObjectFormat,
    reused_pack_bytes: &[u8],
    appended: &[PackInput<'_>],
) -> Result<(Vec<u8>, u32)> {
    assemble_pack_with_verbatim_reuses(format, &[reused_pack_bytes], appended)
}

/// Like [`assemble_pack_with_verbatim_reuse`], but concatenates multiple whole
/// packs before appending fresh entries.
pub fn assemble_pack_with_verbatim_reuses(
    format: ObjectFormat,
    reused_packs: &[&[u8]],
    appended: &[PackInput<'_>],
) -> Result<(Vec<u8>, u32)> {
    let hash_len = format.raw_len();
    let mut reused_count = 0u32;
    let mut capacity = 12 + hash_len + 64 * appended.len();
    for reused_pack_bytes in reused_packs {
        if reused_pack_bytes.len() < 12 + hash_len {
            return Err(GitError::InvalidFormat("reused pack too short".into()));
        }
        if &reused_pack_bytes[..4] != b"PACK" {
            return Err(GitError::InvalidFormat(
                "reused pack has no signature".into(),
            ));
        }
        let version = u32::from_be_bytes([
            reused_pack_bytes[4],
            reused_pack_bytes[5],
            reused_pack_bytes[6],
            reused_pack_bytes[7],
        ]);
        if version != 2 {
            return Err(GitError::Unsupported(format!(
                "reused pack version {version}"
            )));
        }
        let count = u32::from_be_bytes([
            reused_pack_bytes[8],
            reused_pack_bytes[9],
            reused_pack_bytes[10],
            reused_pack_bytes[11],
        ]);
        reused_count = reused_count
            .checked_add(count)
            .ok_or_else(|| GitError::InvalidFormat("too many pack objects".into()))?;
        capacity = capacity.saturating_add(reused_pack_bytes.len().saturating_sub(12 + hash_len));
    }
    let total = reused_count
        .checked_add(appended.len() as u32)
        .ok_or_else(|| GitError::InvalidFormat("too many pack objects".into()))?;

    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&total.to_be_bytes());
    for reused_pack_bytes in reused_packs {
        out.extend_from_slice(&reused_pack_bytes[12..reused_pack_bytes.len() - hash_len]);
    }
    for input in appended {
        write_undeltified_pack_entry(&mut out, input.object)?;
    }
    let checksum = sley_core::digest_bytes(format, &out)?;
    out.extend_from_slice(checksum.as_bytes());
    Ok((out, reused_count))
}

/// Assemble a pack stream by copying already-encoded pack entries verbatim and
/// appending freshly encoded undeltified entries.
pub fn assemble_pack_with_verbatim_entries(
    format: ObjectFormat,
    reused_entries: &[&[u8]],
    appended: &[PackInput<'_>],
) -> Result<(Vec<u8>, u32)> {
    let reused_count = u32::try_from(reused_entries.len())
        .map_err(|_| GitError::InvalidFormat("too many pack objects".into()))?;
    let total = reused_count
        .checked_add(appended.len() as u32)
        .ok_or_else(|| GitError::InvalidFormat("too many pack objects".into()))?;

    let mut capacity = 12 + format.raw_len() + 64 * appended.len();
    for entry in reused_entries {
        capacity = capacity.saturating_add(entry.len());
    }
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&total.to_be_bytes());
    for entry in reused_entries {
        out.extend_from_slice(entry);
    }
    for input in appended {
        write_undeltified_pack_entry(&mut out, input.object)?;
    }
    let checksum = sley_core::digest_bytes(format, &out)?;
    out.extend_from_slice(checksum.as_bytes());
    Ok((out, reused_count))
}

/// Append one undeltified pack entry (type/size varint header + zlib body).
fn write_undeltified_pack_entry(out: &mut Vec<u8>, object: &EncodedObject) -> Result<()> {
    let type_bits: u8 = match object.object_type {
        ObjectType::Commit => 1,
        ObjectType::Tree => 2,
        ObjectType::Blob => 3,
        ObjectType::Tag => 4,
    };
    let mut size = object.body.len() as u64;
    let mut byte = (type_bits << 4) | (size & 0x0f) as u8;
    size >>= 4;
    while size > 0 {
        out.push(byte | 0x80);
        byte = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(byte);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&object.body)?;
    out.extend_from_slice(&encoder.finish()?);
    Ok(())
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
    /// Pack stems (`pack-<checksum>`) that policy says must survive pruning
    /// even if the new pack contains all of their objects.
    retained_pack_stems: Vec<String>,
    /// Whether the freshly written pack should receive a `.promisor` sidecar.
    promisor: bool,
    pack_checksum: ObjectId,
    index_entries: Vec<PackIndexEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct RepackOptions {
    /// Do not borrow objects from alternates (`git repack --local`).
    pub local: bool,
    /// Repack objects that are already in `.keep` / `--keep-pack` packs.
    pub pack_kept_objects: bool,
    /// Explicit `--keep-pack=<name>` pack stems (`pack-<checksum>`).
    pub keep_pack_stems: HashSet<String>,
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
/// `git repack -a`'s gathering rule: pack the reachability closure of `roots`
/// (ref tips, `HEAD`, reflog entries, indexed objects) instead of everything
/// on disk. Borrowed objects (alternates) reachable from the roots are packed
/// into the new local pack like upstream `pack-objects --all` without
/// `--local`; previously-packed objects that are no longer reachable are NOT
/// carried forward (that is how `repack -a -d` drops them). Missing objects
/// are tolerated (stale reflog entries may reference pruned history).
///
/// Returns `Ok(None)` when no roots resolve to any object.
pub fn repack_reachable_objects(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
) -> Result<Option<RepackResult>> {
    repack_reachable_objects_with_options(git_dir, format, roots, &RepackOptions::default())
}

pub fn repack_reachable_objects_with_options(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    options: &RepackOptions,
) -> Result<Option<RepackResult>> {
    let objects_dir = repository_objects_dir(git_dir);
    let database = if options.local {
        FileObjectDatabase::without_alternates(objects_dir.clone(), format)
    } else {
        FileObjectDatabase::new(objects_dir.clone(), format)
    };
    let retained_pack_stems = repack_retained_pack_stems(
        &objects_dir.join("pack"),
        &options.keep_pack_stems,
        !options.pack_kept_objects,
    )?;
    let excluded_oids = if options.pack_kept_objects {
        HashSet::new()
    } else {
        pack_oids_for_stems(&objects_dir.join("pack"), format, &retained_pack_stems)?
    };
    let promisor_oids = promisor_pack_object_ids(&objects_dir, format)?;

    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut objects: Vec<ReachablePackObject> = Vec::new();
    let mut pending: Vec<ObjectId> = roots.to_vec();
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if promisor_oids.contains(&oid) {
            continue;
        }
        let object = match database.read_object(&oid) {
            Ok(object) => object,
            Err(GitError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        match object.object_type {
            ObjectType::Commit => {
                let commit = Commit::parse_ref(format, &object.body)?;
                pending.extend(grafted_parents(&database, &oid, commit.parents));
                pending.push(commit.tree);
            }
            ObjectType::Tree => {
                for entry in TreeEntries::new(format, &object.body) {
                    let entry = entry?;
                    if !entry.is_gitlink() {
                        pending.push(entry.oid);
                    }
                }
            }
            ObjectType::Tag => {
                let tag = Tag::parse_ref(format, &object.body)?;
                pending.push(tag.object);
            }
            ObjectType::Blob => {}
        }
        if !excluded_oids.contains(&oid) {
            objects.push(ReachablePackObject { oid, object });
        }
    }

    // Non-local repacks borrow packed objects from alternates as complete pack
    // sources, while still leaving loose-only alternate objects alone. This
    // matches `pack-objects --all` without `--local`: packed alternate objects
    // are copied into the local consolidated pack, but a loose object in an
    // alternate ODB is not duplicated just because a local tree points at it.
    if !options.local {
        for (alternate, oid) in alternate_packed_object_ids(&objects_dir, format)? {
            if excluded_oids.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            let alternate_db = FileObjectDatabase::without_alternates(alternate, format);
            match alternate_db.read_object(&oid) {
                Ok(object) => objects.push(ReachablePackObject { oid, object }),
                Err(GitError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
    }

    if objects.is_empty() {
        return Ok(None);
    }

    let inputs = pack_inputs(&objects);
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let object_count = written.entries.len();

    // Every pre-existing local pack is superseded under `-a` (their reachable
    // objects are in the new pack; their unreachable ones are being dropped).
    let new_pack_file_name = format!("pack-{}.pack", written.checksum.to_hex());
    let obsolete_packs = existing_pack_files(&objects_dir.join("pack"))?
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) != Some(&new_pack_file_name))
        .collect();

    let packed_oid_set: HashSet<&ObjectId> = written.entries.iter().map(|e| &e.oid).collect();
    let mut packed_loose: Vec<ObjectId> = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .filter(|oid| packed_oid_set.contains(oid))
        .collect();
    packed_loose.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    let pack_checksum = written.checksum;
    let index_entries = written.entries.clone();
    Ok(Some(RepackResult {
        pack: written.pack,
        idx: written.index,
        object_count,
        obsolete_packs,
        packed_loose,
        retained_pack_stems,
        promisor: false,
        pack_checksum,
        index_entries,
    }))
}

fn repack_retained_pack_stems(
    pack_dir: &Path,
    explicit: &HashSet<String>,
    keep_dot_keep: bool,
) -> Result<Vec<String>> {
    let mut stems = explicit.clone();
    if keep_dot_keep {
        for pack_path in existing_pack_files(pack_dir)? {
            if pack_path.with_extension("keep").exists()
                && let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str())
            {
                stems.insert(stem.to_string());
            }
        }
    }
    let mut stems = stems.into_iter().collect::<Vec<_>>();
    stems.sort();
    Ok(stems)
}

fn pack_oids_for_stems(
    pack_dir: &Path,
    format: ObjectFormat,
    stems: &[String],
) -> Result<HashSet<ObjectId>> {
    let wanted: HashSet<&str> = stems.iter().map(String::as_str).collect();
    if wanted.is_empty() {
        return Ok(HashSet::new());
    }
    let mut oids = HashSet::new();
    for pack_path in existing_pack_files(pack_dir)? {
        let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !wanted.contains(stem) {
            continue;
        }
        let index_path = pack_path.with_extension("idx");
        if !index_path.exists() {
            continue;
        }
        let index = PackIndex::parse(&fs::read(index_path)?, format)?;
        oids.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(oids)
}

fn alternate_packed_object_ids(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<(PathBuf, ObjectId)>> {
    let mut oids = Vec::new();
    for alternate in alternate_object_dirs(objects_dir) {
        let mut alternate_oids = HashSet::new();
        collect_packed_object_ids(&alternate.join("pack"), format, &mut alternate_oids)?;
        oids.extend(
            alternate_oids
                .into_iter()
                .map(|oid| (alternate.clone(), oid)),
        );
    }
    oids.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(left.1.as_bytes().cmp(right.1.as_bytes()))
    });
    Ok(oids)
}

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
        retained_pack_stems: Vec::new(),
        promisor: false,
        pack_checksum: written.checksum,
        index_entries: written.entries,
    }))
}

/// Consolidate multiple existing promisor packs into one promisor pack.
///
/// A single promisor pack is left untouched, which preserves its content-derived
/// pack name for callers that expect that exact pack to survive.
pub fn repack_promisor_objects(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<RepackResult>> {
    let objects_dir = repository_objects_dir(git_dir);
    let pack_dir = objects_dir.join("pack");
    let promisor_packs = existing_pack_files(&pack_dir)?
        .into_iter()
        .filter(|path| path.with_extension("promisor").exists())
        .collect::<Vec<_>>();
    if promisor_packs.len() <= 1 {
        return Ok(None);
    }

    let database = FileObjectDatabase::new(objects_dir.clone(), format);
    let mut seen = HashSet::new();
    let mut objects = Vec::new();
    for pack_path in &promisor_packs {
        let index_path = pack_path.with_extension("idx");
        if !index_path.exists() {
            continue;
        }
        let index = PackIndex::parse(&fs::read(index_path)?, format)?;
        for entry in index.entries {
            if !seen.insert(entry.oid) {
                continue;
            }
            objects.push(ReachablePackObject {
                oid: entry.oid,
                object: database.read_object(&entry.oid)?,
            });
        }
    }
    if objects.is_empty() {
        return Ok(None);
    }
    objects.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));

    let inputs = pack_inputs(&objects);
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let object_count = written.entries.len();
    let packed_oid_set: HashSet<&ObjectId> = written.entries.iter().map(|e| &e.oid).collect();
    let mut packed_loose: Vec<ObjectId> = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .filter(|oid| packed_oid_set.contains(oid))
        .collect();
    packed_loose.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    let pack_checksum = written.checksum;
    let index_entries = written.entries.clone();
    Ok(Some(RepackResult {
        pack: written.pack,
        idx: written.index,
        object_count,
        obsolete_packs: promisor_packs,
        packed_loose,
        retained_pack_stems: Vec::new(),
        promisor: true,
        pack_checksum,
        index_entries,
    }))
}

/// Gather only loose objects in `git_dir` and write them into a new pack.
///
/// This is the engine for plain `git repack -d` (without `-a`): existing packs
/// remain in place, and pruning removes only the loose copies that the new pack
/// now serves.
pub fn repack_loose_objects(git_dir: &Path, format: ObjectFormat) -> Result<Option<RepackResult>> {
    let objects_dir = repository_objects_dir(git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);
    let loose_oids = loose_object_ids(&objects_dir, format)?;
    if loose_oids.is_empty() {
        return Ok(None);
    }

    let mut objects = Vec::with_capacity(loose_oids.len());
    for oid in &loose_oids {
        objects.push(ReachablePackObject {
            oid: *oid,
            object: database.read_object(oid)?,
        });
    }

    let inputs = pack_inputs(&objects);
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let object_count = written.entries.len();
    let packed_oid_set: HashSet<&ObjectId> = written.entries.iter().map(|e| &e.oid).collect();
    let mut packed_loose: Vec<ObjectId> = loose_oids
        .into_iter()
        .filter(|oid| packed_oid_set.contains(oid))
        .collect();
    packed_loose.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    let pack_checksum = written.checksum;
    let index_entries = written.entries.clone();
    Ok(Some(RepackResult {
        pack: written.pack,
        idx: written.index,
        object_count,
        obsolete_packs: Vec::new(),
        packed_loose,
        retained_pack_stems: Vec::new(),
        promisor: false,
        pack_checksum,
        index_entries,
    }))
}

/// A local, non-kept, non-cruft pack considered for a geometric rollup,
/// paired with the object count that orders it in the progression.
#[derive(Debug, Clone)]
struct GeometryPack {
    /// Absolute path to the `.pack` file.
    pack_path: PathBuf,
    /// Object ids the pack holds (from its `.idx`).
    oids: Vec<ObjectId>,
    /// `num_objects` weight used to order the progression.
    weight: u64,
    /// True when this pack is a promisor pack (`.promisor` sidecar).
    is_promisor: bool,
}

/// The outcome of a geometric rollup: the new pack (if one was written) plus
/// the rolled-up packs whose objects it now serves.
#[derive(Debug, Clone)]
pub struct GeometricRepackResult {
    /// `Some` when a new pack was written; `None` when nothing needed packing.
    pub result: Option<RepackResult>,
    /// Pack `.pack` paths below the split that may now be removed under `-d`.
    pub rolled_up_packs: Vec<PathBuf>,
}

/// Collect the local non-cruft, non-kept packs eligible for geometric rollup,
/// keyed by promisor-ness, ordered ascending by object count.
fn collect_geometry_packs(
    objects_dir: &Path,
    format: ObjectFormat,
    kept_pack_stems: &HashSet<String>,
) -> Result<Vec<GeometryPack>> {
    let pack_dir = objects_dir.join("pack");
    let mut packs = Vec::new();
    for pack_path in existing_pack_files(&pack_dir)? {
        // Cruft packs (`.mtimes` sidecar) and kept packs are excluded from the
        // progression, matching `pack_geometry_init` in repack-geometry.c.
        if pack_path.with_extension("mtimes").exists() {
            continue;
        }
        if pack_path.with_extension("keep").exists() {
            continue;
        }
        let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if kept_pack_stems.contains(stem) {
            continue;
        }
        let index_path = pack_path.with_extension("idx");
        if !index_path.exists() {
            continue;
        }
        let index = PackIndex::parse(&fs::read(&index_path)?, format)?;
        let oids: Vec<ObjectId> = index.entries.iter().map(|entry| entry.oid).collect();
        let weight = oids.len() as u64;
        packs.push(GeometryPack {
            is_promisor: pack_path.with_extension("promisor").exists(),
            pack_path,
            oids,
            weight,
        });
    }
    // Ascending by weight; pack_path breaks ties deterministically.
    packs.sort_by(|a, b| a.weight.cmp(&b.weight).then(a.pack_path.cmp(&b.pack_path)));
    Ok(packs)
}

/// Port of `compute_pack_geometry_split` (repack-geometry.c): given packs in
/// ascending weight order, return the split index — packs `[0..split)` roll up
/// into one new pack, packs `[split..)` are left alone.
fn compute_geometry_split(packs: &[GeometryPack], split_factor: u64) -> usize {
    let pack_nr = packs.len();
    if pack_nr == 0 {
        return 0;
    }
    // Count packs (descending size) that already form a geometric progression.
    let mut i = pack_nr - 1;
    while i > 0 {
        let ours = packs[i].weight;
        let prev = packs[i - 1].weight;
        if ours < split_factor.saturating_mul(prev) {
            break;
        }
        i -= 1;
    }
    let mut split = i;
    if split != 0 {
        // The top of the last-compared pair can't be in the progression.
        split += 1;
    }

    // Roll up everything below `split`; pulling those into a new pack may break
    // the progression in the heavy half, so absorb heavy-half packs until it
    // holds again.
    let mut total_size: u64 = packs[..split].iter().map(|p| p.weight).sum();
    for pack in &packs[split..] {
        if pack.weight < split_factor.saturating_mul(total_size) {
            split += 1;
            total_size = total_size.saturating_add(pack.weight);
        } else {
            break;
        }
    }
    split
}

/// `git repack --geometric=<factor>`: roll up the smallest packs (plus loose
/// unpacked objects) so the surviving packs form a geometric progression by
/// object count. Objects in the rolled-up packs and loose objects are gathered
/// into one new pack; packs at/above the split are left in place. The new pack
/// excludes objects already served by a left-alone pack.
///
/// Returns the new pack plus the rolled-up pack paths the caller may delete
/// under `-d`. Returns an all-`None`/empty result when nothing needs packing
/// ("Nothing new to pack").
pub fn repack_geometric(
    git_dir: &Path,
    format: ObjectFormat,
    split_factor: u64,
    kept_pack_stems: &HashSet<String>,
) -> Result<GeometricRepackResult> {
    let objects_dir = repository_objects_dir(git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);

    // Promisor packs follow their own progression; the non-promisor packs are
    // the common case the test-suite exercises. Build the rollup from the
    // non-promisor packs plus loose objects.
    let all_packs = collect_geometry_packs(&objects_dir, format, kept_pack_stems)?;
    let packs: Vec<GeometryPack> = all_packs
        .into_iter()
        .filter(|pack| !pack.is_promisor)
        .collect();

    let split = compute_geometry_split(&packs, split_factor);

    let loose_oids = loose_object_ids(&objects_dir, format)?;

    // The objects that end up in the new pack: every object in a rolled-up pack,
    // plus every loose object — but NOT objects already served by a pack left in
    // place (those above the split). This mirrors the `^pack` exclusion markers
    // that repack.c feeds to `pack-objects --stdin-packs`.
    let mut excluded_oids: HashSet<ObjectId> = HashSet::new();
    for pack in &packs[split..] {
        excluded_oids.extend(pack.oids.iter().copied());
    }

    let mut included: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    for pack in &packs[..split] {
        for oid in &pack.oids {
            if excluded_oids.contains(oid) {
                continue;
            }
            if seen.insert(*oid) {
                included.push(*oid);
            }
        }
    }
    for oid in &loose_oids {
        if excluded_oids.contains(oid) {
            continue;
        }
        if seen.insert(*oid) {
            included.push(*oid);
        }
    }

    // "Nothing new to pack": no packs roll up and no loose objects need packing.
    if included.is_empty() {
        return Ok(GeometricRepackResult {
            result: None,
            rolled_up_packs: Vec::new(),
        });
    }

    included.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut objects = Vec::with_capacity(included.len());
    for oid in &included {
        objects.push(ReachablePackObject {
            oid: *oid,
            object: database.read_object(oid)?,
        });
    }

    let inputs = pack_inputs(&objects);
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let object_count = written.entries.len();

    let packed_oid_set: HashSet<&ObjectId> = written.entries.iter().map(|e| &e.oid).collect();
    let mut packed_loose: Vec<ObjectId> = loose_oids
        .into_iter()
        .filter(|oid| packed_oid_set.contains(oid))
        .collect();
    packed_loose.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    let rolled_up_packs: Vec<PathBuf> = packs[..split]
        .iter()
        .map(|pack| pack.pack_path.clone())
        .collect();

    let pack_checksum = written.checksum;
    let index_entries = written.entries.clone();
    Ok(GeometricRepackResult {
        result: Some(RepackResult {
            pack: written.pack,
            idx: written.index,
            object_count,
            obsolete_packs: rolled_up_packs.clone(),
            packed_loose,
            retained_pack_stems: Vec::new(),
            promisor: false,
            pack_checksum,
            index_entries,
        }),
        rolled_up_packs,
    })
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
    install_repack_result_with_bitmap(git_dir, format, result, prune, None, None)
}

/// [`install_repack_result`] that additionally writes a `pack-<checksum>.bitmap`
/// reachability bitmap alongside the new pack when `bitmap_tips` is `Some`.
/// `bitmap_tips` carries the repository's ref tips (peeled to commits): they
/// receive selection preference, mirroring upstream's `NEEDS_BITMAP` flagging of
/// ref tips in `git repack -b` / `pack-objects --write-bitmap-index`.
pub fn install_repack_result_with_bitmap(
    git_dir: &Path,
    format: ObjectFormat,
    result: &RepackResult,
    prune: bool,
    bitmap_tips: Option<&HashSet<ObjectId>>,
    bitmap_pseudo_merge_groups: Option<&[BitmapPseudoMergeGroup]>,
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
    let new_rev_path = pack_dir.join(format!("{pack_name}.rev"));
    let new_index_path = pack_dir.join(format!("{pack_name}.idx"));
    // git writes a `.rev` alongside every repacked pack (`pack.writeReverseIndex`
    // defaults to true). Write it before the `.idx` so the index never becomes
    // visible ahead of its companions, mirroring upstream's finalize order.
    let reverse_index = sley_pack::PackReverseIndex::write(
        format,
        &sley_pack::pack_order_index_positions(&parsed_index.entries),
        &result.pack_checksum,
    )?;
    write_pack_component(&new_pack_path, &result.pack)?;
    write_pack_component(&new_rev_path, &reverse_index)?;
    write_pack_component(&new_index_path, &result.idx)?;
    let new_promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, result.promisor)?;

    if let Some(tips) = bitmap_tips {
        // Build before pruning: the closure walk reads objects through the
        // pre-existing packs/loose store (the new pack holds the same bytes).
        let database = FileObjectDatabase::new(objects_dir.clone(), format);
        if let Some(bitmap) = build_pack_bitmap(
            &database,
            format,
            &result.index_entries,
            &result.pack_checksum,
            tips,
            bitmap_pseudo_merge_groups.unwrap_or(&[]),
        )? {
            // Unlike the pack/idx/rev (content-addressed by the pack
            // checksum), the bitmap depends on selection inputs (e.g.
            // pack.preferBitmapTips), so an existing file must be replaced —
            // write_pack_component's exists-skip would keep a stale selection.
            let bitmap_path = pack_dir.join(format!("{pack_name}.bitmap"));
            remove_file_if_exists(&bitmap_path)?;
            write_pack_component(&bitmap_path, &bitmap)?;
        }
    }

    if !prune {
        return Ok(());
    }

    // Prune based on the objects the new pack's *index* can resolve (what reads use
    // once the old packs are gone), not just what the pack contains — so a stale
    // pack is never removed for an object the new index cannot serve.
    let present: HashSet<ObjectId> = parsed_index.entries.iter().map(|entry| entry.oid).collect();

    prune_obsolete_pack_paths(
        &objects_dir,
        format,
        &result.obsolete_packs,
        &new_pack_path,
        &result.retained_pack_stems,
        result.promisor,
    )?;
    prune_loose_objects(&objects_dir, format, result.packed_loose.iter(), &present)?;
    if result.promisor && new_promisor_path.is_none() {
        return Err(GitError::InvalidFormat(
            "promisor repack did not write sidecar".into(),
        ));
    }
    Ok(())
}

/// Install a [`repack_geometric`] result: write the new pack, then under `prune`
/// remove EXACTLY the rolled-up packs (those below the geometric split) plus the
/// loose objects now packed. Unlike [`install_repack_result`], packs left in
/// place above the split are never removed even though some of their objects may
/// also live in the new pack.
pub fn install_geometric_repack_result(
    git_dir: &Path,
    format: ObjectFormat,
    geometric: &GeometricRepackResult,
    prune: bool,
    bitmap_tips: Option<&HashSet<ObjectId>>,
) -> Result<()> {
    let Some(result) = geometric.result.as_ref() else {
        return Ok(());
    };
    let objects_dir = repository_objects_dir(git_dir);
    let pack_dir = objects_dir.join("pack");
    fs::create_dir_all(&pack_dir)?;

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
    let new_rev_path = pack_dir.join(format!("{pack_name}.rev"));
    let new_index_path = pack_dir.join(format!("{pack_name}.idx"));
    let reverse_index = sley_pack::PackReverseIndex::write(
        format,
        &sley_pack::pack_order_index_positions(&parsed_index.entries),
        &result.pack_checksum,
    )?;
    write_pack_component(&new_pack_path, &result.pack)?;
    write_pack_component(&new_rev_path, &reverse_index)?;
    write_pack_component(&new_index_path, &result.idx)?;

    if let Some(tips) = bitmap_tips {
        let database = FileObjectDatabase::new(objects_dir.clone(), format);
        if let Some(bitmap) = build_pack_bitmap(
            &database,
            format,
            &result.index_entries,
            &result.pack_checksum,
            tips,
            &[],
        )? {
            let bitmap_path = pack_dir.join(format!("{pack_name}.bitmap"));
            remove_file_if_exists(&bitmap_path)?;
            write_pack_component(&bitmap_path, &bitmap)?;
        }
    }

    if !prune {
        return Ok(());
    }

    // Remove exactly the rolled-up packs (below the split). Never touch packs
    // left in place above the split.
    for pack_path in &geometric.rolled_up_packs {
        if *pack_path == new_pack_path {
            continue;
        }
        if pack_path.with_extension("keep").exists() {
            continue;
        }
        remove_file_if_exists(pack_path)?;
        remove_file_if_exists(&pack_path.with_extension("idx"))?;
        for ext in ["rev", "mtimes", "bitmap", "promisor"] {
            remove_file_if_exists(&pack_path.with_extension(ext))?;
        }
    }

    // Drop loose copies now served by the new pack.
    let present: HashSet<ObjectId> = parsed_index.entries.iter().map(|entry| entry.oid).collect();
    prune_loose_objects(&objects_dir, format, result.packed_loose.iter(), &present)?;

    // A multi-pack-index that references any removed pack is now stale.
    let removed_stems: HashSet<String> = geometric
        .rolled_up_packs
        .iter()
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    prune_stale_multi_pack_index(&pack_dir, format, &removed_stems)?;
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

/// The UNIX-seconds mtime of a path, or `0` when unavailable.
fn path_mtime_secs(path: &Path) -> u32 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|dur| dur.as_secs() as u32)
        .unwrap_or(0)
}

/// The bytes of one cruft `.mtimes` pack plus its sidecars and checksum, ready
/// to install under `objects/pack/`.
#[derive(Debug, Clone)]
pub struct CruftPack {
    pub pack: Vec<u8>,
    pub idx: Vec<u8>,
    pub rev: Vec<u8>,
    pub mtimes: Vec<u8>,
    pub checksum: ObjectId,
    /// Object ids the cruft pack holds (its surviving unreachable set).
    pub oids: Vec<ObjectId>,
}

/// Outcome of `git repack --cruft`: the reachable pack (if any) plus the cruft
/// `.mtimes` pack of surviving unreachable objects.
#[derive(Debug, Clone)]
pub struct CruftRepackResult {
    /// The all-into-one reachable pack, or `None` when nothing is reachable.
    pub reachable: Option<RepackResult>,
    /// The cruft pack of unreachable objects, or `None` when there are none.
    pub cruft: Option<CruftPack>,
    /// Pre-existing non-cruft, non-kept pack `.pack` paths superseded by the
    /// reachable pack (removed under `-d`).
    pub obsolete_packs: Vec<PathBuf>,
    /// Pre-existing cruft `.pack` paths whose objects are now in the new cruft
    /// pack (removed under `-d`).
    pub obsolete_cruft_packs: Vec<PathBuf>,
    retained_pack_stems: Vec<String>,
}

/// Gather every object id on disk together with the best (max) mtime of any
/// copy: a packed object contributes its pack's mtime (or its own recorded
/// mtime inside a cruft pack), a loose object contributes its file mtime.
pub fn object_mtimes_on_disk_pub(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<HashMap<ObjectId, u32>> {
    object_mtimes_on_disk(objects_dir, format)
}

fn object_mtimes_on_disk(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<HashMap<ObjectId, u32>> {
    let mut mtimes: HashMap<ObjectId, u32> = HashMap::new();
    let mut record = |oid: ObjectId, mtime: u32| {
        mtimes
            .entry(oid)
            .and_modify(|existing| {
                if mtime > *existing {
                    *existing = mtime;
                }
            })
            .or_insert(mtime);
    };

    let pack_dir = objects_dir.join("pack");
    if let Ok(entries) = fs::read_dir(&pack_dir) {
        let mut idx_paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("idx") {
                idx_paths.push(path);
            }
        }
        idx_paths.sort();
        for idx_path in idx_paths {
            let pack_path = idx_path.with_extension("pack");
            if !pack_path.exists() {
                continue;
            }
            let index = PackIndex::parse(&fs::read(&idx_path)?, format)?;
            let mtimes_path = idx_path.with_extension("mtimes");
            let pack_object_mtimes: Option<Vec<u32>> =
                fs::read(&mtimes_path).ok().and_then(|bytes| {
                    sley_pack::PackMtimes::parse(&bytes, format, index.entries.len())
                        .ok()
                        .map(|parsed| parsed.mtimes)
                });
            let pack_mtime = path_mtime_secs(&pack_path);
            for (pos, entry) in index.entries.iter().enumerate() {
                let mtime = pack_object_mtimes
                    .as_ref()
                    .and_then(|table| table.get(pos).copied())
                    .unwrap_or(pack_mtime);
                record(entry.oid, mtime);
            }
        }
    }

    let store = LooseObjectStore::new(objects_dir.to_path_buf(), format);
    for oid in loose_object_ids(objects_dir, format)? {
        let path = store.object_path(&oid)?;
        record(oid, path_mtime_secs(&path));
    }
    Ok(mtimes)
}

/// Public wrapper over [`build_cruft_pack`] for the `--expire-to` limbo pack.
pub fn build_cruft_pack_pub(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    survivors: &HashMap<ObjectId, u32>,
) -> Result<Option<CruftPack>> {
    build_cruft_pack(database, format, survivors)
}

/// Build the cruft `.mtimes` pack from the surviving unreachable objects and
/// their timestamps.
fn build_cruft_pack(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    survivors: &HashMap<ObjectId, u32>,
) -> Result<Option<CruftPack>> {
    if survivors.is_empty() {
        return Ok(None);
    }
    let mut ordered: Vec<(ObjectId, u32)> = survivors.iter().map(|(o, m)| (*o, *m)).collect();
    ordered.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let mut oids: Vec<ObjectId> = Vec::with_capacity(ordered.len());
    let mut objects: Vec<Arc<EncodedObject>> = Vec::with_capacity(ordered.len());
    let mut mtime_by_oid: HashMap<ObjectId, u32> = HashMap::with_capacity(ordered.len());
    for (oid, mtime) in ordered {
        match database.read_object(&oid) {
            Ok(object) => {
                oids.push(oid);
                objects.push(object);
                mtime_by_oid.insert(oid, mtime);
            }
            Err(GitError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
    }
    if oids.is_empty() {
        return Ok(None);
    }

    let inputs: Vec<PackInput<'_>> = oids
        .iter()
        .zip(&objects)
        .map(|(oid, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect();
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;

    // `.mtimes` table is in lexicographic (index/fanout) order.
    let mut sorted_entries: Vec<&sley_pack::PackIndexEntry> = written.entries.iter().collect();
    sorted_entries.sort_by(|a, b| a.oid.as_bytes().cmp(b.oid.as_bytes()));
    let mtimes_table: Vec<u32> = sorted_entries
        .iter()
        .map(|entry| mtime_by_oid.get(&entry.oid).copied().unwrap_or(0))
        .collect();
    let positions = sley_pack::pack_order_index_positions(&written.entries);
    let rev = sley_pack::PackReverseIndex::write(format, &positions, &written.checksum)?;
    let mtimes = sley_pack::PackMtimes::write(format, &mtimes_table, &written.checksum)?;

    let mut cruft_oids: Vec<ObjectId> = sorted_entries.iter().map(|e| e.oid).collect();
    cruft_oids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(Some(CruftPack {
        pack: written.pack,
        idx: written.index,
        rev,
        mtimes,
        checksum: written.checksum,
        oids: cruft_oids,
    }))
}

/// `git repack --cruft [--cruft-expiration=<t>] [-d]`: pack the reachable
/// closure of `roots` into one new pack, then collect every unreachable object
/// into a `.mtimes`-stamped cruft pack (honouring `cruft_expiration`). The
/// caller installs the result and, under `-d`, removes the superseded non-cruft
/// and old cruft packs.
///
/// Mirrors builtin/repack.c's PACK_CRUFT path + repack-cruft.c `write_cruft_pack`
/// without the per-pack stdin protocol: unreachable objects are everything on
/// disk minus the reachable set.
pub fn repack_cruft(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    cruft_expiration: Option<u32>,
) -> Result<CruftRepackResult> {
    repack_cruft_with_options(
        git_dir,
        format,
        roots,
        cruft_expiration,
        &RepackOptions::default(),
    )
}

pub fn repack_cruft_with_options(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    cruft_expiration: Option<u32>,
    options: &RepackOptions,
) -> Result<CruftRepackResult> {
    let objects_dir = repository_objects_dir(git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);
    let pack_dir = objects_dir.join("pack");
    let retained_pack_stems = repack_retained_pack_stems(
        &pack_dir,
        &options.keep_pack_stems,
        !options.pack_kept_objects,
    )?;
    let excluded_oids = if options.pack_kept_objects {
        HashSet::new()
    } else {
        pack_oids_for_stems(&pack_dir, format, &retained_pack_stems)?
    };
    let promisor_oids = promisor_pack_object_ids(&objects_dir, format)?;
    let database = if promisor_oids.is_empty() {
        database
    } else {
        database.with_promisor_remote_present(true)
    };

    // Reachable closure → the new "reachable" pack.
    let mut reachable_ids = collect_reachable_object_ids_excluding_promised_missing(
        &database,
        format,
        roots.iter().copied(),
        &promisor_oids,
    )?;
    reachable_ids.retain(|oid| !excluded_oids.contains(oid));
    let reachable_result = if reachable_ids.is_empty() {
        None
    } else {
        let mut ids: Vec<ObjectId> = reachable_ids.iter().copied().collect();
        ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let mut objects = Vec::with_capacity(ids.len());
        for oid in &ids {
            match database.read_object(oid) {
                Ok(object) => objects.push(ReachablePackObject { oid: *oid, object }),
                Err(GitError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        if objects.is_empty() {
            None
        } else {
            let inputs = pack_inputs(&objects);
            let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
            let packed_set: HashSet<&ObjectId> = written.entries.iter().map(|e| &e.oid).collect();
            let mut packed_loose: Vec<ObjectId> = loose_object_ids(&objects_dir, format)?
                .into_iter()
                .filter(|oid| packed_set.contains(oid))
                .collect();
            packed_loose.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            Some(RepackResult {
                pack: written.pack,
                idx: written.index,
                object_count: written.entries.len(),
                obsolete_packs: Vec::new(),
                packed_loose,
                retained_pack_stems: Vec::new(),
                promisor: false,
                pack_checksum: written.checksum,
                index_entries: written.entries,
            })
        }
    };

    // Unreachable objects = everything on disk minus the reachable set, stamped
    // with their best mtime.
    let mut survivors: HashMap<ObjectId, u32> = object_mtimes_on_disk(&objects_dir, format)?
        .into_iter()
        .filter(|(oid, _)| {
            !reachable_ids.contains(oid)
                && !excluded_oids.contains(oid)
                && !promisor_oids.contains(oid)
        })
        .collect();

    // Expiration: rescue older objects reachable from a recent one, drop the rest.
    if let Some(expiration) = cruft_expiration {
        rescue_and_expire_cruft_objects(&database, format, &mut survivors, expiration)?;
    }

    let cruft = build_cruft_pack(&database, format, &survivors)?;

    // The packs the reachable+cruft packs supersede: every pre-existing
    // non-kept pack. Cruft packs are tracked separately.
    let mut obsolete_packs = Vec::new();
    let mut obsolete_cruft_packs = Vec::new();
    for pack_path in existing_pack_files(&pack_dir)? {
        if let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str())
            && retained_pack_stems.iter().any(|retained| retained == stem)
        {
            continue;
        }
        if pack_path.with_extension("keep").exists() {
            continue;
        }
        if pack_path.with_extension("mtimes").exists() {
            obsolete_cruft_packs.push(pack_path);
        } else {
            obsolete_packs.push(pack_path);
        }
    }

    Ok(CruftRepackResult {
        reachable: reachable_result,
        cruft,
        obsolete_packs,
        obsolete_cruft_packs,
        retained_pack_stems,
    })
}

/// Apply `--cruft-expiration` over the survivor map in place: starting from the
/// recent candidates (mtime strictly newer than `expiration`), walk reachability
/// and rescue every dependency at the cutoff mtime; drop older candidates that
/// no recent object reaches. Mirrors the pack-objects cruft expiry traversal.
fn rescue_and_expire_cruft_objects(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    survivors: &mut HashMap<ObjectId, u32>,
    expiration: u32,
) -> Result<()> {
    let recent: Vec<ObjectId> = survivors
        .iter()
        .filter(|(_, mtime)| **mtime > expiration)
        .map(|(oid, _)| *oid)
        .collect();

    let mut keep: HashSet<ObjectId> = HashSet::new();
    let mut pending: Vec<ObjectId> = recent.clone();
    while let Some(oid) = pending.pop() {
        if !keep.insert(oid) {
            continue;
        }
        let Ok(object) = database.read_object(&oid) else {
            continue;
        };
        match object.object_type {
            ObjectType::Commit => {
                if let Ok(commit) = Commit::parse_ref(format, &object.body) {
                    pending.extend(commit.parents.iter().copied());
                    pending.push(commit.tree);
                }
            }
            ObjectType::Tree => {
                for entry in TreeEntries::new(format, &object.body).flatten() {
                    if !entry.is_gitlink() {
                        pending.push(entry.oid);
                    }
                }
            }
            ObjectType::Tag => {
                if let Ok(tag) = Tag::parse_ref(format, &object.body) {
                    pending.push(tag.object);
                }
            }
            ObjectType::Blob => {}
        }
    }

    // Drop any survivor that is neither recent nor rescued; rescued-but-older
    // objects keep their recorded mtime (already >= 0), recent ones unchanged.
    survivors.retain(|oid, mtime| *mtime > expiration || keep.contains(oid));
    Ok(())
}

/// Install a [`repack_cruft`] result: write the reachable pack and the cruft
/// `.mtimes` pack, then under `prune` remove the superseded non-cruft packs, old
/// cruft packs, and the loose objects now served.
pub fn install_cruft_repack_result(
    git_dir: &Path,
    format: ObjectFormat,
    result: &CruftRepackResult,
    prune: bool,
) -> Result<()> {
    let objects_dir = repository_objects_dir(git_dir);
    let pack_dir = objects_dir.join("pack");
    fs::create_dir_all(&pack_dir)?;

    // Names of packs we are about to remove (so we never delete the new ones).
    let new_reachable_name = result
        .reachable
        .as_ref()
        .map(|r| format!("pack-{}.pack", r.pack_checksum.to_hex()));
    let new_cruft_name = result
        .cruft
        .as_ref()
        .map(|c| format!("pack-{}.pack", c.checksum.to_hex()));

    // Write the reachable pack (idx + rev + pack), content-addressed.
    if let Some(reachable) = result.reachable.as_ref() {
        let parsed_index = PackIndex::parse(&reachable.idx, format)?;
        let pack_name = format!("pack-{}", reachable.pack_checksum.to_hex());
        let reverse_index = sley_pack::PackReverseIndex::write(
            format,
            &sley_pack::pack_order_index_positions(&parsed_index.entries),
            &reachable.pack_checksum,
        )?;
        write_pack_component(&pack_dir.join(format!("{pack_name}.pack")), &reachable.pack)?;
        write_pack_component(&pack_dir.join(format!("{pack_name}.rev")), &reverse_index)?;
        write_pack_component(&pack_dir.join(format!("{pack_name}.idx")), &reachable.idx)?;
    }

    // Write the cruft pack (pack + rev + mtimes + idx).
    if let Some(cruft) = result.cruft.as_ref() {
        let pack_name = format!("pack-{}", cruft.checksum.to_hex());
        write_pack_component(&pack_dir.join(format!("{pack_name}.pack")), &cruft.pack)?;
        write_pack_component(&pack_dir.join(format!("{pack_name}.rev")), &cruft.rev)?;
        write_pack_component(&pack_dir.join(format!("{pack_name}.mtimes")), &cruft.mtimes)?;
        write_pack_component(&pack_dir.join(format!("{pack_name}.idx")), &cruft.idx)?;
    }

    if !prune {
        return Ok(());
    }

    // Objects now served by the new packs.
    let mut present: HashSet<ObjectId> = HashSet::new();
    if let Some(reachable) = result.reachable.as_ref() {
        present.extend(reachable.index_entries.iter().map(|e| e.oid));
    }
    if let Some(cruft) = result.cruft.as_ref() {
        present.extend(cruft.oids.iter().copied());
    }

    // Remove superseded non-cruft + old cruft packs (skip the new ones).
    let mut removed_stems: HashSet<String> = HashSet::new();
    for pack_path in result
        .obsolete_packs
        .iter()
        .chain(result.obsolete_cruft_packs.iter())
    {
        let file_name = pack_path.file_name().and_then(|n| n.to_str());
        if file_name == new_reachable_name.as_deref() || file_name == new_cruft_name.as_deref() {
            continue;
        }
        if let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str())
            && result
                .retained_pack_stems
                .iter()
                .any(|retained| retained == stem)
        {
            continue;
        }
        if pack_path.with_extension("keep").exists() {
            continue;
        }
        if pack_path.with_extension("promisor").exists() {
            continue;
        }
        if let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str()) {
            removed_stems.insert(stem.to_string());
        }
        remove_file_if_exists(pack_path)?;
        remove_file_if_exists(&pack_path.with_extension("idx"))?;
        for ext in ["rev", "mtimes", "bitmap", "promisor"] {
            remove_file_if_exists(&pack_path.with_extension(ext))?;
        }
    }

    // Drop loose objects now in a new pack.
    let loose_now_packed: Vec<ObjectId> = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .filter(|oid| present.contains(oid))
        .collect();
    prune_loose_objects(&objects_dir, format, loose_now_packed.iter(), &present)?;

    prune_stale_multi_pack_index(&pack_dir, format, &removed_stems)?;
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
    prune_unreachable_loose_with_reachability(git_dir, format, roots, delete, false)
}

/// Like [`prune_unreachable_loose`], but missing links encountered while walking
/// reachable roots are ignored. `git gc` uses this mode for pre-existing broken
/// unreachable commits/trees/tags: the broken object itself is kept when recent,
/// but its absent children do not make housekeeping fail.
pub fn prune_unreachable_loose_tolerating_missing<I>(
    git_dir: &Path,
    format: ObjectFormat,
    roots: I,
    delete: bool,
) -> Result<Vec<ObjectId>>
where
    I: IntoIterator<Item = ObjectId>,
{
    prune_unreachable_loose_with_reachability(git_dir, format, roots, delete, true)
}

fn prune_unreachable_loose_with_reachability<I>(
    git_dir: &Path,
    format: ObjectFormat,
    roots: I,
    delete: bool,
    tolerate_missing: bool,
) -> Result<Vec<ObjectId>>
where
    I: IntoIterator<Item = ObjectId>,
{
    let objects_dir = repository_objects_dir(git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);
    let reachable = if tolerate_missing {
        collect_reachable_object_ids_tolerating_missing(&database, format, roots)?
    } else {
        collect_reachable_object_ids(&database, format, roots)?
    };

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
    let oids = loose_object_id_set(objects_dir, format)?;
    let mut oids = oids.into_iter().collect::<Vec<_>>();
    oids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(oids)
}

fn loose_object_id_set(objects_dir: &Path, format: ObjectFormat) -> Result<HashSet<ObjectId>> {
    let mut oids = HashSet::new();
    collect_loose_object_ids(objects_dir, format, &mut oids)?;
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
fn prune_obsolete_pack_paths(
    objects_dir: &Path,
    format: ObjectFormat,
    packs: &[PathBuf],
    keep: &Path,
    retained_pack_stems: &[String],
    prune_promisor: bool,
) -> Result<()> {
    prune_pack_paths_matching(
        objects_dir,
        format,
        packs.iter(),
        keep,
        retained_pack_stems,
        prune_promisor,
        |_| Ok(true),
    )
}

fn prune_pack_paths_matching<'a>(
    objects_dir: &Path,
    format: ObjectFormat,
    packs: impl IntoIterator<Item = &'a PathBuf>,
    keep: &Path,
    retained_pack_stems: &[String],
    prune_promisor: bool,
    mut should_prune: impl FnMut(&Path) -> Result<bool>,
) -> Result<()> {
    let pack_dir = objects_dir.join("pack");
    let keep_stem = keep.file_stem().map(|stem| stem.to_owned());
    let retained_pack_stems: HashSet<&str> =
        retained_pack_stems.iter().map(String::as_str).collect();
    let mut removed_stems: HashSet<String> = HashSet::new();

    for pack_path in packs {
        if pack_path == keep {
            continue;
        }
        let Some(stem) = pack_path.file_stem() else {
            continue;
        };
        if Some(stem) == keep_stem.as_deref() {
            continue;
        }
        if let Some(stem) = stem.to_str()
            && retained_pack_stems.contains(stem)
        {
            continue;
        }
        if pack_path.with_extension("keep").exists() {
            continue;
        }
        if pack_path.with_extension("promisor").exists() && !prune_promisor {
            continue;
        }
        if !should_prune(pack_path)? {
            continue;
        }
        remove_file_if_exists(pack_path)?;
        remove_file_if_exists(&pack_path.with_extension("idx"))?;
        for ext in ["rev", "mtimes", "bitmap", "promisor"] {
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
    trailer_offset: Option<u64>,
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
        } else if let Some(offset) = next_offset {
            offset
        } else {
            trailer_offset.ok_or_else(|| {
                GitError::InvalidFormat("pack size unavailable for final indexed object".into())
            })?
        },
        delta_base_oid,
    })
}

fn scan_pack_offsets_without_index(
    format: ObjectFormat,
    pack: &[u8],
    target_offset: u64,
) -> Result<Option<u64>> {
    let trailer_len = format.raw_len();
    if pack.len() < 12 + trailer_len {
        return Err(GitError::InvalidFormat("pack file too short".into()));
    }
    let trailer_offset = pack.len() - trailer_len;
    let checksum = sley_core::digest_bytes(format, &pack[..trailer_offset])?;
    let expected = ObjectId::from_raw(format, &pack[trailer_offset..])?;
    if checksum != expected {
        return Err(GitError::InvalidFormat(format!(
            "pack checksum mismatch: expected {expected}, got {checksum}"
        )));
    }
    if &pack[..4] != b"PACK" {
        return Err(GitError::InvalidFormat("missing PACK signature".into()));
    }
    let version = u32_be(&pack[4..8]);
    if version != 2 && version != 3 {
        return Err(GitError::Unsupported(format!("pack version {version}")));
    }

    let count = u32_be(&pack[8..12]);
    let mut cursor = 12usize;
    for _ in 0..count {
        let entry_offset = cursor as u64;
        let first = pack_next_byte(pack, &mut cursor)?;
        let kind = (first >> 4) & 0x07;
        let mut byte = first;
        while byte & 0x80 != 0 {
            byte = pack_next_byte(pack, &mut cursor)?;
        }
        match kind {
            1..=4 => {}
            6 => {
                parse_ofs_delta_base_offset(pack, &mut cursor, entry_offset)?;
            }
            7 => {
                parse_ref_delta_base_oid(format, pack, &mut cursor)?;
            }
            _ => {
                return Err(GitError::InvalidFormat(format!(
                    "invalid pack object kind {kind}"
                )));
            }
        }
        if cursor > trailer_offset {
            return Err(GitError::InvalidFormat(
                "pack entry extends past checksum".into(),
            ));
        }
        let consumed = inflate_pack_member_len(&pack[cursor..trailer_offset])?;
        if consumed == 0 {
            return Err(GitError::InvalidFormat(
                "empty compressed pack entry".into(),
            ));
        }
        cursor = cursor
            .checked_add(consumed)
            .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
        if cursor > trailer_offset {
            return Err(GitError::InvalidFormat(
                "pack entry extends past checksum".into(),
            ));
        }
        if entry_offset == target_offset {
            return Ok(Some(cursor as u64));
        }
    }
    if cursor != trailer_offset {
        return Err(GitError::InvalidFormat(format!(
            "pack has {} trailing bytes before checksum",
            trailer_offset - cursor
        )));
    }
    Ok(None)
}

fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn inflate_pack_member_len(compressed: &[u8]) -> Result<usize> {
    let mut decompress = Decompress::new(true);
    let mut input = compressed;
    let mut consumed_total = 0usize;
    let mut out = [0u8; 8192];
    loop {
        let before_in = decompress.total_in();
        let before_out = decompress.total_out();
        let status = decompress
            .decompress(input, &mut out, FlushDecompress::None)
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
    visit: F,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
    F: FnMut(&ObjectId, &Arc<EncodedObject>),
{
    walk_reachable_objects_with_cut(reader, format, starts, excluded, &HashSet::new(), visit)
}

/// [`walk_reachable_objects`] with an additional `cut` set: commits in `cut`
/// are visited (their trees and blobs too) but their parents are not followed,
/// mirroring a shallow client's view of its own history during negotiation.
fn walk_reachable_objects_with_cut<R, I, F>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
    cut: &HashSet<ObjectId>,
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
            let object = reader.read_object(&oid).map_err(|err| {
                with_missing_object_context(err, oid, MissingObjectContext::Traversal)
            })?;
            match object.object_type {
                ObjectType::Commit => {
                    let (tree, parents) = {
                        let commit = Commit::parse_ref(format, &object.body)?;
                        (commit.tree, commit.parents)
                    };
                    visit(&oid, &object);
                    if !cut.contains(&oid) {
                        for parent in grafted_parents(reader, &oid, parents).into_iter().rev() {
                            pending.push(parent);
                        }
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

fn walk_reachable_objects_tolerating_missing<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let mut seen = HashSet::new();
    let mut pending: Vec<ObjectId> = starts.into_iter().collect();
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = match reader
            .read_object(&oid)
            .map_err(|err| with_missing_object_context(err, oid, MissingObjectContext::Traversal))
        {
            Ok(object) => object,
            Err(GitError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        match object.object_type {
            ObjectType::Commit => {
                let commit = Commit::parse_ref(format, &object.body)?;
                pending.extend(grafted_parents(reader, &oid, commit.parents));
                pending.push(commit.tree);
            }
            ObjectType::Tree => {
                for entry in TreeEntries::new(format, &object.body) {
                    let entry = entry?;
                    if !entry.is_gitlink() {
                        pending.push(entry.oid);
                    }
                }
            }
            ObjectType::Tag => {
                let tag = Tag::parse_ref(format, &object.body)?;
                pending.push(tag.object);
            }
            ObjectType::Blob => {}
        }
    }
    Ok(seen)
}

// ===== reachability bitmaps (.bitmap write + consult) =====

#[derive(Debug, Clone)]
pub struct BitmapPseudoMergeGroup {
    pub commits: Vec<ObjectId>,
    pub exclude_selected: bool,
    pub partition: Option<BitmapPseudoMergePartition>,
}

#[derive(Debug, Clone)]
pub struct BitmapPseudoMergePartition {
    pub max_merges: usize,
    pub decay: f64,
    pub sample_rate: f64,
}

/// Bit accessors over a `Vec<u64>` bitset using git's bitmap convention:
/// bit `i` lives in word `i / 64` at bit `i % 64` (LSB-first within a word).
fn bitset_get(words: &[u64], position: u32) -> bool {
    let word = (position / 64) as usize;
    word < words.len() && words[word] & (1u64 << (position % 64)) != 0
}

fn bitset_set(words: &mut [u64], position: u32) {
    let word = (position / 64) as usize;
    if word < words.len() {
        words[word] |= 1u64 << (position % 64);
    }
}

fn bitset_or(acc: &mut [u64], other: &[u64]) {
    for (dst, src) in acc.iter_mut().zip(other) {
        *dst |= *src;
    }
}

fn bitset_is_subset(needles: &[u64], haystack: &[u64]) -> bool {
    needles
        .iter()
        .zip(haystack)
        .all(|(needle, hay)| needle & !hay == 0)
}

/// Sorted set-bit positions of a bitset (the inverse of repeated [`bitset_set`]).
fn bitset_positions(words: &[u64]) -> Vec<u32> {
    let mut positions = Vec::new();
    for (word_index, word) in words.iter().enumerate() {
        let mut remaining = *word;
        while remaining != 0 {
            let bit = remaining.trailing_zeros();
            positions.push(word_index as u32 * 64 + bit);
            remaining &= remaining - 1;
        }
    }
    positions
}

/// Committer timestamp (epoch seconds) of a commit identity line
/// (`Name <email> <timestamp> <tz>`); 0 when unparseable, matching git's
/// tolerance for bogus dates during bitmap commit selection.
fn commit_identity_timestamp(identity: &[u8]) -> i64 {
    let mut fields = identity.rsplitn(3, |byte| *byte == b' ');
    let _tz = fields.next();
    fields
        .next()
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Upstream `next_commit_index` (pack-bitmap-write.c): the spacing schedule for
/// bitmap commit selection over the date-descending commit list.
fn bitmap_next_commit_index(idx: u32) -> u32 {
    const MIN_COMMITS: u32 = 100;
    const MAX_COMMITS: u32 = 5000;
    const MUST_REGION: u32 = 100;
    const MIN_REGION: u32 = 20000;

    if idx <= MUST_REGION {
        return 0;
    }
    if idx <= MIN_REGION {
        let offset = idx - MUST_REGION;
        return offset.min(MIN_COMMITS);
    }
    let offset = idx - MIN_REGION;
    offset.clamp(MIN_COMMITS, MAX_COMMITS)
}

/// Builds a serialised `.bitmap` for the pack described by `index_entries` /
/// `pack_checksum`, mirroring upstream pack-bitmap-write.c:
///
/// * commit selection walks the pack's commits in committer-date-descending
///   order through [`bitmap_next_commit_index`]'s spacing schedule, preferring
///   `preferred_tips` (ref tips — upstream's `NEEDS_BITMAP`) and merge commits
///   inside each window;
/// * each selected commit stores its full reachability closure (commits, trees,
///   blobs) as pack-order bit positions (no XOR compression — `xor_offset` 0 is
///   valid on disk and what readers see after resolution anyway).
///
/// Returns `Ok(None)` — mirroring upstream's warn-and-skip — when the pack
/// lacks full closure (a reachable object is missing from it).
pub fn build_pack_bitmap(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    index_entries: &[PackIndexEntry],
    pack_checksum: &ObjectId,
    preferred_tips: &HashSet<ObjectId>,
    pseudo_merge_groups: &[BitmapPseudoMergeGroup],
) -> Result<Option<Vec<u8>>> {
    // `index_entries` carries no ordering guarantee (writer provenance is in
    // pack-write order); bit numbering follows pack (offset) order.
    let mut by_offset: Vec<usize> = (0..index_entries.len()).collect();
    by_offset.sort_by_key(|&slot| index_entries[slot].offset);
    let bit_order: Vec<ObjectId> = by_offset
        .into_iter()
        .map(|slot| index_entries[slot].oid)
        .collect();
    build_reachability_bitmap(
        db,
        format,
        pack_checksum,
        &bit_order,
        preferred_tips,
        pseudo_merge_groups,
    )
}

/// [`build_pack_bitmap`]'s multi-pack sibling: builds the serialised
/// `multi-pack-index-<checksum>.bitmap` for `midx_entries`, with bits in
/// pseudo-pack order (preferred pack first, then pack id, then offset — the
/// same order [`MultiPackIndex::write_with_reverse_index`] records in `RIDX`)
/// and the midx checksum in the BITM checksum field.
pub fn build_midx_bitmap(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    midx_entries: &[sley_pack::MultiPackIndexEntry],
    midx_checksum: &ObjectId,
    preferred_pack: u32,
    preferred_tips: &HashSet<ObjectId>,
    pseudo_merge_groups: &[BitmapPseudoMergeGroup],
) -> Result<Option<Vec<u8>>> {
    let mut pseudo: Vec<usize> = (0..midx_entries.len()).collect();
    pseudo.sort_by_key(|&slot| {
        let entry = &midx_entries[slot];
        (
            entry.pack_int_id != preferred_pack,
            entry.pack_int_id,
            entry.offset,
        )
    });
    let bit_order: Vec<ObjectId> = pseudo
        .into_iter()
        .map(|slot| midx_entries[slot].oid)
        .collect();
    build_reachability_bitmap(
        db,
        format,
        midx_checksum,
        &bit_order,
        preferred_tips,
        pseudo_merge_groups,
    )
}

/// Upstream `bitmap_builder_init`'s `num_maximal` counter (pack-bitmap-write.c):
/// walk the first-parent ancestry of the selected commits, children before
/// parents, propagating per-commit "which selected commits reach me" masks.
/// A commit counts as maximal when it is selected, or when distinct selected
/// lineages converge on it (its mask gains bits its last contributing child
/// did not carry). Only the count is needed (for the trace2 data event), so no
/// reverse-edge bookkeeping is kept.
fn bitmap_num_maximal_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    selected: &[ObjectId],
) -> Result<usize> {
    // First-parent subgraph reachable from the selected commits.
    let mut first_parent: HashMap<ObjectId, Option<ObjectId>> = HashMap::new();
    let mut stack: Vec<ObjectId> = selected.to_vec();
    while let Some(oid) = stack.pop() {
        if first_parent.contains_key(&oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        let commit = Commit::parse_ref(format, &object.body)?;
        let parent = grafted_parents(db, &oid, commit.parents).first().copied();
        first_parent.insert(oid, parent);
        if let Some(parent) = parent {
            stack.push(parent);
        }
    }
    // Children-before-parents order (Kahn over the single first-parent edge).
    let mut pending_children: HashMap<ObjectId, usize> = HashMap::new();
    for parent in first_parent.values().flatten() {
        *pending_children.entry(*parent).or_default() += 1;
    }
    let word_count = selected.len().div_ceil(64);
    struct MaximalEnt {
        mask: Vec<u64>,
        maximal: bool,
    }
    let mut ents: HashMap<ObjectId, MaximalEnt> = HashMap::new();
    for (bit, oid) in selected.iter().enumerate() {
        let ent = ents.entry(*oid).or_insert_with(|| MaximalEnt {
            mask: vec![0u64; word_count],
            maximal: true,
        });
        ent.mask[bit / 64] |= 1u64 << (bit % 64);
        ent.maximal = true;
    }
    let mut queue: Vec<ObjectId> = first_parent
        .keys()
        .filter(|oid| pending_children.get(*oid).copied().unwrap_or(0) == 0)
        .copied()
        .collect();
    let mut num_maximal = 0usize;
    while let Some(oid) = queue.pop() {
        if let Some(ent) = ents.remove(&oid) {
            if ent.maximal {
                num_maximal += 1;
            }
            if let Some(Some(parent)) = first_parent.get(&oid) {
                match ents.entry(*parent) {
                    std::collections::hash_map::Entry::Vacant(vacant) => {
                        // Fresh parent mask: c_not_p, !p_not_c -> not maximal.
                        vacant.insert(MaximalEnt {
                            mask: ent.mask.clone(),
                            maximal: false,
                        });
                    }
                    std::collections::hash_map::Entry::Occupied(mut occupied) => {
                        let parent_ent = occupied.get_mut();
                        let c_not_p = ent
                            .mask
                            .iter()
                            .zip(&parent_ent.mask)
                            .any(|(child, parent)| child & !parent != 0);
                        if c_not_p {
                            let p_not_c = parent_ent
                                .mask
                                .iter()
                                .zip(&ent.mask)
                                .any(|(parent, child)| parent & !child != 0);
                            for (parent, child) in parent_ent.mask.iter_mut().zip(&ent.mask) {
                                *parent |= child;
                            }
                            parent_ent.maximal = p_not_c;
                        }
                    }
                }
            }
        }
        if let Some(Some(parent)) = first_parent.get(&oid)
            && let Some(remaining) = pending_children.get_mut(parent)
        {
            *remaining -= 1;
            if *remaining == 0 {
                queue.push(*parent);
            }
        }
    }
    Ok(num_maximal)
}

/// Shared write half: `bit_order` lists every covered object's oid in bit
/// order (pack order for a single pack, pseudo-pack order for a midx);
/// `checksum` fills the BITM checksum field (pack checksum / midx checksum).
fn build_reachability_bitmap(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    checksum: &ObjectId,
    bit_order: &[ObjectId],
    preferred_tips: &HashSet<ObjectId>,
    pseudo_merge_groups: &[BitmapPseudoMergeGroup],
) -> Result<Option<Vec<u8>>> {
    if bit_order.is_empty() || bit_order.len() > u32::MAX as usize {
        return Ok(None);
    }
    let object_count = bit_order.len();

    // The on-disk entry position space is the oid-sorted lookup order (.idx /
    // midx OIDL); derive each bit-order slot's rank there.
    let mut oid_sorted: Vec<u32> = (0..object_count as u32).collect();
    oid_sorted.sort_by(|&left, &right| {
        bit_order[left as usize]
            .as_bytes()
            .cmp(bit_order[right as usize].as_bytes())
    });
    let mut index_position = vec![0u32; object_count];
    for (position, &slot) in oid_sorted.iter().enumerate() {
        index_position[slot as usize] = position as u32;
    }
    let mut oid_to_pack = HashMap::with_capacity(object_count);
    for (pack_pos, oid) in bit_order.iter().enumerate() {
        oid_to_pack.insert(*oid, pack_pos as u32);
    }

    // Object types in bit order; commits also collect (date, parent count).
    let mut object_types = Vec::with_capacity(object_count);
    struct IndexedCommit {
        oid: ObjectId,
        pack_pos: u32,
        index_pos: u32,
        date: i64,
        parent_count: usize,
    }
    let mut indexed_commits = Vec::new();
    for (pack_pos, oid) in bit_order.iter().enumerate() {
        // Type via the header fast path: blobs (the bulk of most packs) never
        // need their bodies inflated here.
        let object_type = match db.read_object_header(oid)? {
            Some((object_type, _)) => object_type,
            None => db.read_object(oid)?.object_type,
        };
        object_types.push(object_type);
        if object_type == ObjectType::Commit {
            let object = db.read_object(oid)?;
            let commit = Commit::parse_ref(format, &object.body)?;
            indexed_commits.push(IndexedCommit {
                oid: *oid,
                pack_pos: pack_pos as u32,
                index_pos: index_position[pack_pos],
                date: commit_identity_timestamp(commit.committer),
                parent_count: grafted_parents(db, oid, commit.parents).len(),
            });
        }
    }

    // Selection: date-descending, then the spacing schedule.
    indexed_commits.sort_by_key(|commit| std::cmp::Reverse(commit.date));
    let mut selected: Vec<&IndexedCommit> = Vec::new();
    let commit_count = indexed_commits.len() as u32;
    if commit_count < 100 {
        selected.extend(indexed_commits.iter());
    } else {
        let mut i = 0u32;
        loop {
            let next = bitmap_next_commit_index(i);
            if i + next >= commit_count {
                break;
            }
            let mut chosen = &indexed_commits[(i + next) as usize];
            if next > 0 {
                for j in 0..=next {
                    let candidate = &indexed_commits[(i + j) as usize];
                    if preferred_tips.contains(&candidate.oid) {
                        chosen = candidate;
                        break;
                    }
                    if candidate.parent_count >= 2 {
                        chosen = candidate;
                    }
                }
            }
            selected.push(chosen);
            i += next + 1;
        }
    }

    // Trace2 selection counters (upstream bitmap_builder_init): emitted before
    // the closure walk, like upstream emits them before building the ewah
    // bitmaps. Computing num_maximal_commits needs its own first-parent walk,
    // so it only runs when the trace2 event target is active.
    if std::env::var_os("GIT_TRACE2_EVENT").is_some() {
        let selected_oids: Vec<ObjectId> = selected.iter().map(|commit| commit.oid).collect();
        let num_maximal = bitmap_num_maximal_commits(db, format, &selected_oids)?;
        sley_core::trace2::data("pack-bitmap-write", "num_selected_commits", selected.len());
        sley_core::trace2::data("pack-bitmap-write", "num_maximal_commits", num_maximal);
        let reusable_pseudo_merges = pseudo_merge_groups
            .iter()
            .filter(|group| !group.exclude_selected)
            .count();
        sley_core::trace2::data(
            "pack-bitmap-write",
            "building_bitmaps_pseudo_merge_reused",
            reusable_pseudo_merges,
        );
    }

    // Reachability closures, oldest-first so newer walks stop at memoised
    // older selected commits.
    let word_count = object_count.div_ceil(64);
    let mut memo: HashMap<ObjectId, Arc<Vec<u64>>> = HashMap::new();
    for commit in selected.iter().rev() {
        let Some(acc) =
            bitmap_commit_closure(db, format, &[commit.oid], &oid_to_pack, word_count, &memo)?
        else {
            return Ok(None);
        };
        memo.insert(commit.oid, Arc::new(acc));
    }

    let mut writer = PackBitmapWriter::new(format, *checksum, &object_types)?;
    for commit in &selected {
        let words = match memo.get(&commit.oid) {
            Some(words) => words,
            None => continue,
        };
        writer.add_commit(commit.pack_pos, commit.index_pos, &bitset_positions(words))?;
    }
    if !pseudo_merge_groups.is_empty() {
        let selected_oids: HashSet<ObjectId> = selected.iter().map(|commit| commit.oid).collect();
        for group in pseudo_merge_groups {
            let mut commits = Vec::new();
            for oid in &group.commits {
                if group.exclude_selected && selected_oids.contains(oid) {
                    continue;
                }
                let Some(&pack_pos) = oid_to_pack.get(oid) else {
                    continue;
                };
                if object_types.get(pack_pos as usize) != Some(&ObjectType::Commit) {
                    continue;
                }
                commits.push((*oid, pack_pos));
            }
            if commits.is_empty() {
                continue;
            }
            if let Some(partition) = &group.partition {
                let mut start = 0usize;
                for merge_index in 0..partition.max_merges {
                    if start >= commits.len() {
                        break;
                    }
                    let size = bitmap_pseudo_merge_group_size(
                        partition.max_merges,
                        partition.decay,
                        commits.len(),
                        merge_index,
                    );
                    let end = if size < 8 {
                        commits.len()
                    } else {
                        start.saturating_add(size).min(commits.len())
                    };
                    let sample_stride = if partition.sample_rate <= 0.0 {
                        usize::MAX
                    } else {
                        ((1.0 / partition.sample_rate) as usize).max(1)
                    };
                    let sampled: Vec<(ObjectId, u32)> = commits[start..end]
                        .iter()
                        .enumerate()
                        .filter(|(offset, _candidate)| *offset % sample_stride == 0)
                        .map(|(_offset, candidate)| *candidate)
                        .collect();
                    if !sampled.is_empty()
                        && !bitmap_add_pseudo_merge(
                            &mut writer,
                            db,
                            format,
                            &sampled,
                            &oid_to_pack,
                            word_count,
                            &memo,
                        )?
                    {
                        return Ok(None);
                    }
                    start = end;
                    if end >= commits.len() {
                        break;
                    }
                }
            } else if !bitmap_add_pseudo_merge(
                &mut writer,
                db,
                format,
                &commits,
                &oid_to_pack,
                word_count,
                &memo,
            )? {
                return Ok(None);
            }
        }
    }
    writer.write().map(Some)
}

fn bitmap_pseudo_merge_group_size(
    max_merges: usize,
    decay: f64,
    unstable_len: usize,
    index: usize,
) -> usize {
    let mut scale = 0.0;
    for n in 0..max_merges {
        scale += 1.0 / ((n + 1) as f64).powf(decay);
    }
    if scale == 0.0 {
        return 0;
    }
    ((unstable_len as f64 / scale) / ((index + 1) as f64).powf(decay) + 0.5) as usize
}

fn bitmap_add_pseudo_merge(
    writer: &mut PackBitmapWriter,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commits: &[(ObjectId, u32)],
    oid_to_pack: &HashMap<ObjectId, u32>,
    word_count: usize,
    memo: &HashMap<ObjectId, Arc<Vec<u64>>>,
) -> Result<bool> {
    let roots: Vec<ObjectId> = commits.iter().map(|(oid, _position)| *oid).collect();
    let Some(words) = bitmap_commit_closure(db, format, &roots, oid_to_pack, word_count, memo)?
    else {
        return Ok(false);
    };
    let commit_positions: Vec<u32> = commits.iter().map(|(_oid, position)| *position).collect();
    writer.add_pseudo_merge(&commit_positions, &bitset_positions(&words))?;
    Ok(true)
}

fn bitmap_commit_closure(
    db: &impl ObjectReader,
    format: ObjectFormat,
    roots: &[ObjectId],
    oid_to_pack: &HashMap<ObjectId, u32>,
    word_count: usize,
    memo: &HashMap<ObjectId, Arc<Vec<u64>>>,
) -> Result<Option<Vec<u64>>> {
    let mut acc = vec![0u64; word_count];
    let mut pending = roots.to_vec();
    while let Some(oid) = pending.pop() {
        let Some(&pack_pos) = oid_to_pack.get(&oid) else {
            eprintln!(
                "warning: Failed to write bitmap index. Packfile doesn't have full closure (object {oid} is missing)"
            );
            return Ok(None);
        };
        if bitset_get(&acc, pack_pos) {
            continue;
        }
        if let Some(stored) = memo.get(&oid) {
            bitset_or(&mut acc, stored);
            continue;
        }
        bitset_set(&mut acc, pack_pos);
        let object = db.read_object(&oid)?;
        let parsed = Commit::parse_ref(format, &object.body)?;
        pending.extend(grafted_parents(db, &oid, parsed.parents));
        if !bitmap_mark_tree(db, format, &parsed.tree, oid_to_pack, &mut acc)? {
            return Ok(None);
        }
    }
    Ok(Some(acc))
}

/// Marks `tree` and everything below it (sub-trees, blobs) in `acc`, skipping
/// already-set bits (their closure is already covered). Returns `false` when an
/// object is missing from the pack (no full closure), after warning.
fn bitmap_mark_tree(
    db: &impl ObjectReader,
    format: ObjectFormat,
    tree: &ObjectId,
    oid_to_pack: &HashMap<ObjectId, u32>,
    acc: &mut [u64],
) -> Result<bool> {
    let Some(&pack_pos) = oid_to_pack.get(tree) else {
        eprintln!(
            "warning: Failed to write bitmap index. Packfile doesn't have full closure (object {tree} is missing)"
        );
        return Ok(false);
    };
    if bitset_get(acc, pack_pos) {
        return Ok(true);
    }
    bitset_set(acc, pack_pos);
    let object = db.read_object(tree)?;
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        if entry.is_gitlink() {
            continue;
        }
        if entry.is_tree() {
            if !bitmap_mark_tree(db, format, &entry.oid, oid_to_pack, acc)? {
                return Ok(false);
            }
        } else {
            let Some(&blob_pos) = oid_to_pack.get(&entry.oid) else {
                eprintln!(
                    "warning: Failed to write bitmap index. Packfile doesn't have full closure (object {} is missing)",
                    entry.oid
                );
                return Ok(false);
            };
            bitset_set(acc, blob_pos);
        }
    }
    Ok(true)
}

/// A pack's `.bitmap` loaded for consultation: oid <-> pack-position mappings,
/// resolved (XOR-expanded) per-commit reachability bitsets, and the four object
/// type bitmaps. Bit numbering follows pack order throughout.
pub struct LoadedPackBitmap {
    object_count: u32,
    oid_to_pack: HashMap<ObjectId, u32>,
    pack_to_oid: Vec<ObjectId>,
    commit_words: HashMap<ObjectId, Arc<Vec<u64>>>,
    pseudo_merges: Vec<LoadedPseudoMerge>,
    commits: Vec<u64>,
    trees: Vec<u64>,
    blobs: Vec<u64>,
    tags: Vec<u64>,
}

struct LoadedPseudoMerge {
    commits: Arc<Vec<u64>>,
    bitmap: Arc<Vec<u64>>,
}

impl LoadedPackBitmap {
    pub fn object_count(&self) -> u32 {
        self.object_count
    }

    /// Pack-order position of `oid`, when the object is in the bitmapped pack.
    pub fn pack_position(&self, oid: &ObjectId) -> Option<u32> {
        self.oid_to_pack.get(oid).copied()
    }

    pub fn oid_at(&self, position: u32) -> Option<&ObjectId> {
        self.pack_to_oid.get(position as usize)
    }

    /// The resolved reachability bitset stored for `oid`, when it was one of
    /// the writer's selected commits.
    pub fn bitmap_for_commit(&self, oid: &ObjectId) -> Option<&Arc<Vec<u64>>> {
        self.commit_words.get(oid)
    }

    /// Oids of every commit with a stored bitmap entry (unordered).
    pub fn bitmapped_commits(&self) -> impl Iterator<Item = &ObjectId> {
        self.commit_words.keys()
    }

    pub fn pseudo_merge_count(&self) -> usize {
        self.pseudo_merges.len()
    }

    pub fn pseudo_merge_words(&self, index: usize) -> Option<(&[u64], &[u64])> {
        self.pseudo_merges
            .get(index)
            .map(|merge| (merge.commits.as_slice(), merge.bitmap.as_slice()))
    }

    /// The type bitmap for `object_type` (bit per pack position).
    pub fn type_words(&self, object_type: ObjectType) -> &[u64] {
        match object_type {
            ObjectType::Commit => &self.commits,
            ObjectType::Tree => &self.trees,
            ObjectType::Blob => &self.blobs,
            ObjectType::Tag => &self.tags,
        }
    }

    fn word_count(&self) -> usize {
        (self.object_count as usize).div_ceil(64)
    }
}

/// Loads the single-pack `.bitmap` of `objects_dir/pack`, if a valid one
/// exists. Scans `pack-*.bitmap` files (sorted, first valid wins, like
/// upstream's "first bitmap" behaviour), requires the sibling `.idx`, and
/// verifies the recorded pack checksum. Any unreadable/corrupt bitmap yields
/// `Ok(None)` — consumers fall back to a regular object walk, mirroring
/// upstream's warn-and-ignore on bitmap load failure.
pub fn load_pack_bitmap(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<LoadedPackBitmap>> {
    let pack_dir = objects_dir.join("pack");
    if !pack_dir.exists() {
        return Ok(None);
    }
    // A multi-pack bitmap wins over single-pack bitmaps, like upstream's
    // open_bitmap trying the midx first.
    if let Some(bitmap) = load_incremental_midx_bitmap(&pack_dir, format)? {
        return Ok(Some(bitmap));
    }
    if let Some(bitmap) = load_midx_bitmap(&pack_dir, format)? {
        return Ok(Some(bitmap));
    }
    let mut bitmap_paths = Vec::new();
    for entry in fs::read_dir(&pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("bitmap")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("pack-"))
        {
            bitmap_paths.push(path);
        }
    }
    bitmap_paths.sort();
    for bitmap_path in bitmap_paths {
        match load_pack_bitmap_file(&bitmap_path, format) {
            Ok(Some(bitmap)) => return Ok(Some(bitmap)),
            Ok(None) | Err(_) => continue,
        }
    }
    Ok(None)
}

fn load_incremental_midx_bitmap(
    pack_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<LoadedPackBitmap>> {
    let chain = read_incremental_midx_chain(pack_dir)?;
    if chain.is_empty() {
        return Ok(None);
    }
    let midx_dir = pack_dir.join("multi-pack-index.d");
    if !chain.iter().any(|checksum| {
        midx_dir
            .join(format!("multi-pack-index-{checksum}.bitmap"))
            .exists()
    }) {
        return Ok(None);
    }

    let mut pack_to_oid = Vec::new();
    for checksum in &chain {
        let path = midx_dir.join(format!("multi-pack-index-{checksum}.midx"));
        let Ok(bytes) = fs::read(path) else {
            return Ok(None);
        };
        let Ok(midx) = MultiPackIndex::parse(&bytes, format) else {
            return Ok(None);
        };
        let mut positions: Vec<usize> = match &midx.reverse_index {
            Some(reverse) => reverse.iter().map(|position| *position as usize).collect(),
            None => {
                let mut positions: Vec<usize> = (0..midx.objects.len()).collect();
                positions.sort_by_key(|&position| {
                    let entry = &midx.objects[position];
                    (entry.pack_int_id, entry.offset)
                });
                positions
            }
        };
        for position in positions.drain(..) {
            let Some(entry) = midx.objects.get(position) else {
                return Ok(None);
            };
            pack_to_oid.push(entry.oid);
        }
    }

    let object_count = pack_to_oid.len();
    if object_count == 0 || object_count > u32::MAX as usize {
        return Ok(None);
    }
    let mut oid_to_pack = HashMap::with_capacity(object_count);
    for (position, oid) in pack_to_oid.iter().enumerate() {
        oid_to_pack.insert(*oid, position as u32);
    }

    let Some(objects_dir) = pack_dir.parent() else {
        return Ok(None);
    };
    let db = FileObjectDatabase::new(objects_dir.to_path_buf(), format);
    let word_count = object_count.div_ceil(64);
    let mut commits = vec![0u64; word_count];
    let mut trees = vec![0u64; word_count];
    let mut blobs = vec![0u64; word_count];
    let mut tags = vec![0u64; word_count];
    let mut commit_oids = Vec::new();
    for (position, oid) in pack_to_oid.iter().enumerate() {
        let Ok(Some((object_type, _size))) = db.read_object_header(oid) else {
            return Ok(None);
        };
        let position = position as u32;
        match object_type {
            ObjectType::Commit => {
                bitset_set(&mut commits, position);
                commit_oids.push(*oid);
            }
            ObjectType::Tree => bitset_set(&mut trees, position),
            ObjectType::Blob => bitset_set(&mut blobs, position),
            ObjectType::Tag => bitset_set(&mut tags, position),
        }
    }

    let mut loaded = LoadedPackBitmap {
        object_count: object_count as u32,
        oid_to_pack,
        pack_to_oid,
        commit_words: HashMap::new(),
        pseudo_merges: Vec::new(),
        commits,
        trees,
        blobs,
        tags,
    };
    for oid in commit_oids {
        let result = bitmap_reachable(&loaded, &db, format, &[oid], true)?;
        if result.extended.is_empty() {
            loaded.commit_words.insert(oid, Arc::new(result.words));
        }
    }
    Ok(Some(loaded))
}

/// Loads `multi-pack-index-<checksum>.bitmap` when the pack directory has a
/// multi-pack-index with a `RIDX` chunk (the bit-order permutation) and a
/// matching bitmap file. Returns `Ok(None)` — never an error — on any missing
/// or unusable piece, so callers fall through to single-pack bitmaps.
fn load_midx_bitmap(pack_dir: &Path, format: ObjectFormat) -> Result<Option<LoadedPackBitmap>> {
    let midx_path = pack_dir.join("multi-pack-index");
    if !midx_path.exists() {
        return Ok(None);
    }
    let Ok(midx_bytes) = fs::read(&midx_path) else {
        return Ok(None);
    };
    if midx_has_bad_ridx_chunk(&midx_bytes, format) {
        eprintln!("error: multi-pack-index reverse-index chunk is the wrong size");
        eprintln!("warning: multi-pack bitmap is missing required reverse index");
        return Ok(None);
    }
    let midx = match MultiPackIndex::parse(&midx_bytes, format) {
        Ok(midx) => midx,
        Err(GitError::InvalidFormat(message))
            if message == "multi-pack-index reverse-index chunk is the wrong size" =>
        {
            eprintln!("error: {message}");
            eprintln!("warning: multi-pack bitmap is missing required reverse index");
            return Ok(None);
        }
        Err(_) => return Ok(None),
    };
    let bitmap_path = pack_dir.join(format!(
        "multi-pack-index-{}.bitmap",
        midx.checksum.to_hex()
    ));
    if !bitmap_path.exists() {
        return Ok(None);
    }
    let object_count = midx.objects.len();
    // Upstream `load_midx_revindex`: prefer the midx's own RIDX chunk unless
    // GIT_TEST_MIDX_READ_RIDX=0 disables it, else fall back to the separate
    // `multi-pack-index-<checksum>.rev` file; a trace2 data event records
    // which source supplied the permutation.
    let read_ridx_chunk = env::var("GIT_TEST_MIDX_READ_RIDX")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let reverse_index: Vec<u32> = match (&midx.reverse_index, read_ridx_chunk) {
        (Some(chunk), true) => {
            sley_core::trace2::data("load_midx_revindex", "source", "midx");
            chunk.clone()
        }
        _ => {
            let rev_path =
                pack_dir.join(format!("multi-pack-index-{}.rev", midx.checksum.to_hex()));
            let Ok(rev_bytes) = fs::read(&rev_path) else {
                // Without the RIDX permutation the bit numbering is unknown.
                return Ok(None);
            };
            let Ok(parsed_rev) =
                sley_pack::PackReverseIndex::parse(&rev_bytes, format, object_count)
            else {
                return Ok(None);
            };
            sley_core::trace2::data("load_midx_revindex", "source", "rev");
            parsed_rev.positions
        }
    };
    let Ok(bitmap_bytes) = fs::read(&bitmap_path) else {
        return Ok(None);
    };
    let parsed = match PackBitmapIndex::parse(&bitmap_bytes, format, object_count) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None),
    };
    if parsed.pack_checksum != midx.checksum {
        return Ok(None);
    }

    // midx.objects is in lookup (oid-sorted) order; RIDX maps bit positions
    // to lookup positions.
    let mut pack_to_oid = Vec::with_capacity(object_count);
    for &midx_pos in &reverse_index {
        let Some(entry) = midx.objects.get(midx_pos as usize) else {
            return Ok(None);
        };
        pack_to_oid.push(entry.oid);
    }
    let mut oid_to_pack = HashMap::with_capacity(object_count);
    for (pack_pos, oid) in pack_to_oid.iter().enumerate() {
        oid_to_pack.insert(*oid, pack_pos as u32);
    }
    match assemble_loaded_bitmap(parsed, object_count, pack_to_oid, oid_to_pack, |position| {
        midx.objects.get(position).map(|entry| entry.oid)
    }) {
        Ok(loaded) => Ok(Some(loaded)),
        Err(_) => Ok(None),
    }
}

fn midx_has_bad_ridx_chunk(bytes: &[u8], format: ObjectFormat) -> bool {
    let hash_len = format.raw_len();
    if bytes.len() < 12 + 12 + hash_len || &bytes[..4] != b"MIDX" {
        return false;
    }
    let chunk_count = bytes[6] as usize;
    let table_len = match (chunk_count + 1).checked_mul(12) {
        Some(table_len) => table_len,
        None => return false,
    };
    let table_end = match 12usize.checked_add(table_len) {
        Some(table_end) if table_end <= bytes.len().saturating_sub(hash_len) => table_end,
        _ => return false,
    };
    let mut entries = Vec::with_capacity(chunk_count + 1);
    let mut cursor = 12usize;
    while cursor < table_end {
        let id = [
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ];
        let mut raw_offset = [0u8; 8];
        raw_offset.copy_from_slice(&bytes[cursor + 4..cursor + 12]);
        entries.push((id, u64::from_be_bytes(raw_offset) as usize));
        cursor += 12;
    }
    let mut oidf = None;
    let mut ridx = None;
    for pair in entries.windows(2) {
        let start = pair[0].1;
        let end = pair[1].1;
        if end < start || end > bytes.len().saturating_sub(hash_len) {
            return false;
        }
        match &pair[0].0 {
            b"OIDF" => oidf = Some((start, end)),
            b"RIDX" => ridx = Some((start, end)),
            _ => {}
        }
    }
    let Some((oidf_start, oidf_end)) = oidf else {
        return false;
    };
    let Some((ridx_start, ridx_end)) = ridx else {
        return false;
    };
    if oidf_end.saturating_sub(oidf_start) != 256 * 4 {
        return false;
    }
    let object_count_start = oidf_end - 4;
    let object_count = u32::from_be_bytes([
        bytes[object_count_start],
        bytes[object_count_start + 1],
        bytes[object_count_start + 2],
        bytes[object_count_start + 3],
    ]) as usize;
    ridx_end.saturating_sub(ridx_start) != object_count.saturating_mul(4)
}

fn load_pack_bitmap_file(
    bitmap_path: &Path,
    format: ObjectFormat,
) -> Result<Option<LoadedPackBitmap>> {
    let index_path = bitmap_path.with_extension("idx");
    if !index_path.exists() {
        return Ok(None);
    }
    let index = PackIndex::parse(&fs::read(&index_path)?, format)?;
    let object_count = index.entries.len();
    let parsed = PackBitmapIndex::parse(&fs::read(bitmap_path)?, format, object_count)?;
    if parsed.pack_checksum != index.pack_checksum {
        return Ok(None);
    }

    let mut pack_order: Vec<u32> = (0..object_count as u32).collect();
    pack_order.sort_by_key(|index_pos| index.entries[*index_pos as usize].offset);
    let mut pack_to_oid = Vec::with_capacity(object_count);
    for index_pos in &pack_order {
        pack_to_oid.push(index.entries[*index_pos as usize].oid);
    }
    let mut oid_to_pack = HashMap::with_capacity(object_count);
    for (pack_pos, oid) in pack_to_oid.iter().enumerate() {
        oid_to_pack.insert(*oid, pack_pos as u32);
    }

    assemble_loaded_bitmap(parsed, object_count, pack_to_oid, oid_to_pack, |position| {
        index.entries.get(position).map(|entry| entry.oid)
    })
    .map(Some)
}

/// Shared tail of the bitmap loaders: expands the type bitmaps, resolves the
/// per-commit entries (XOR offsets reference earlier entries in file order),
/// and maps each entry's lookup-order position back to a commit oid via
/// `lookup_oid`.
fn assemble_loaded_bitmap(
    parsed: PackBitmapIndex,
    object_count: usize,
    pack_to_oid: Vec<ObjectId>,
    oid_to_pack: HashMap<ObjectId, u32>,
    lookup_oid: impl Fn(usize) -> Option<ObjectId>,
) -> Result<LoadedPackBitmap> {
    let word_count = object_count.div_ceil(64);
    let expand = |bitmap: &sley_pack::EwahBitmap| -> Result<Vec<u64>> {
        let mut words = bitmap.to_words()?;
        words.resize(word_count, 0);
        Ok(words)
    };

    let mut resolved: Vec<Arc<Vec<u64>>> = Vec::with_capacity(parsed.entries.len());
    let mut commit_words = HashMap::with_capacity(parsed.entries.len());
    for (entry_index, entry) in parsed.entries.iter().enumerate() {
        let mut words = expand(&entry.bitmap)?;
        if entry.xor_offset > 0 {
            let base_index = entry_index - entry.xor_offset as usize;
            let base = &resolved[base_index];
            for (dst, src) in words.iter_mut().zip(base.iter()) {
                *dst ^= *src;
            }
        }
        let words = Arc::new(words);
        resolved.push(Arc::clone(&words));
        let commit_oid = lookup_oid(entry.object_position as usize)
            .ok_or_else(|| GitError::InvalidFormat("bitmap entry position out of range".into()))?;
        commit_words.insert(commit_oid, words);
    }
    let mut pseudo_merges = Vec::with_capacity(parsed.pseudo_merges.len());
    for merge in &parsed.pseudo_merges {
        pseudo_merges.push(LoadedPseudoMerge {
            commits: Arc::new(expand(&merge.commits)?),
            bitmap: Arc::new(expand(&merge.bitmap)?),
        });
    }

    Ok(LoadedPackBitmap {
        object_count: object_count as u32,
        oid_to_pack,
        pack_to_oid,
        commit_words,
        pseudo_merges,
        commits: expand(&parsed.type_bitmaps.commits)?,
        trees: expand(&parsed.type_bitmaps.trees)?,
        blobs: expand(&parsed.type_bitmaps.blobs)?,
        tags: expand(&parsed.type_bitmaps.tags)?,
    })
}

/// Result of a bitmap-assisted reachability walk: pack-position bits for
/// in-pack objects plus the "extended" objects encountered outside the
/// bitmapped pack (in first-seen order, like upstream's extended index).
pub struct BitmapWalkResult {
    pub words: Vec<u64>,
    pub extended: Vec<(ObjectId, ObjectType)>,
    pub pseudo_merges_satisfied: usize,
    pub pseudo_merges_cascades: usize,
}

impl BitmapWalkResult {
    /// Removes everything reachable in `haves` from this result.
    pub fn subtract(&mut self, haves: &BitmapWalkResult) {
        for (dst, src) in self.words.iter_mut().zip(haves.words.iter()) {
            *dst &= !*src;
        }
        let have_ext: HashSet<ObjectId> = haves.extended.iter().map(|(oid, _)| *oid).collect();
        self.extended.retain(|(oid, _)| !have_ext.contains(oid));
    }
}

/// Computes the set of objects reachable from `roots` using stored bitmaps
/// where available and a fill-in object walk where not — the consult half of
/// the bitmap engine (upstream `find_objects` + `fill_in_bitmap`).
///
/// Roots may be any object type; tag chains are peeled with every tag object
/// itself included, like the pending-object handling in
/// `prepare_bitmap_walk`. When `include_objects` is false only commits are
/// walked (tree contents of fill-in commits are not marked) — callers that
/// only count/enumerate commits mask with the commit type bitmap, so the
/// extra non-commit bits OR-ed in from stored (closed) bitmaps are harmless.
pub fn bitmap_reachable(
    bitmap: &LoadedPackBitmap,
    db: &impl ObjectReader,
    format: ObjectFormat,
    roots: &[ObjectId],
    include_objects: bool,
) -> Result<BitmapWalkResult> {
    let mut walk = BitmapFillWalk {
        bitmap,
        words: vec![0u64; bitmap.word_count()],
        extended: Vec::new(),
        extended_seen: HashSet::new(),
    };
    let mut commit_stack: Vec<ObjectId> = Vec::new();

    for root in roots {
        let mut oid = *root;
        // Peel tag chains, marking each tag object on the way.
        loop {
            let object = db.read_object(&oid)?;
            match object.object_type {
                ObjectType::Tag => {
                    walk.mark(&oid, ObjectType::Tag);
                    let tag = Tag::parse_ref(format, &object.body)?;
                    oid = tag.object;
                }
                ObjectType::Commit => {
                    commit_stack.push(oid);
                    break;
                }
                ObjectType::Tree => {
                    walk.mark_tree_closure(db, format, &oid)?;
                    break;
                }
                ObjectType::Blob => {
                    walk.mark(&oid, ObjectType::Blob);
                    break;
                }
            }
        }
    }

    while let Some(oid) = commit_stack.pop() {
        if let Some(position) = bitmap.pack_position(&oid) {
            if bitset_get(&walk.words, position) {
                continue;
            }
            if let Some(stored) = bitmap.bitmap_for_commit(&oid) {
                bitset_or(&mut walk.words, stored);
                continue;
            }
            bitset_set(&mut walk.words, position);
        } else {
            if walk.extended_seen.contains(&oid) {
                continue;
            }
            walk.extended_seen.insert(oid);
            walk.extended.push((oid, ObjectType::Commit));
        }
        let object = db.read_object(&oid)?;
        let commit = Commit::parse_ref(format, &object.body)?;
        commit_stack.extend(grafted_parents(db, &oid, commit.parents));
        if include_objects {
            walk.mark_tree_closure(db, format, &commit.tree)?;
        }
    }

    let (pseudo_merges_satisfied, pseudo_merges_cascades) =
        bitmap_cascade_pseudo_merges(bitmap, &mut walk.words);

    Ok(BitmapWalkResult {
        words: walk.words,
        extended: walk.extended,
        pseudo_merges_satisfied,
        pseudo_merges_cascades,
    })
}

fn bitmap_cascade_pseudo_merges(bitmap: &LoadedPackBitmap, words: &mut [u64]) -> (usize, usize) {
    if bitmap.pseudo_merges.is_empty() {
        return (0, 0);
    }
    let mut satisfied = vec![false; bitmap.pseudo_merges.len()];
    let mut total = 0usize;
    loop {
        let mut any = false;
        for (index, merge) in bitmap.pseudo_merges.iter().enumerate() {
            if satisfied[index] || !bitset_is_subset(merge.commits.as_slice(), words) {
                continue;
            }
            bitset_or(words, merge.bitmap.as_slice());
            satisfied[index] = true;
            any = true;
            total += 1;
        }
        if !any {
            break;
        }
    }
    (total, usize::from(total > 0))
}

struct BitmapFillWalk<'a> {
    bitmap: &'a LoadedPackBitmap,
    words: Vec<u64>,
    extended: Vec<(ObjectId, ObjectType)>,
    extended_seen: HashSet<ObjectId>,
}

impl BitmapFillWalk<'_> {
    /// Marks one object; returns false when it was already marked.
    fn mark(&mut self, oid: &ObjectId, object_type: ObjectType) -> bool {
        if let Some(position) = self.bitmap.pack_position(oid) {
            if bitset_get(&self.words, position) {
                return false;
            }
            bitset_set(&mut self.words, position);
            true
        } else {
            if !self.extended_seen.insert(*oid) {
                return false;
            }
            self.extended.push((*oid, object_type));
            true
        }
    }

    /// Marks `tree` and everything below it, skipping subtrees already marked
    /// (a set in-pack bit means its closure is covered: either it came from a
    /// stored — closed — bitmap, or this walk already expanded it).
    fn mark_tree_closure(
        &mut self,
        db: &impl ObjectReader,
        format: ObjectFormat,
        tree: &ObjectId,
    ) -> Result<()> {
        if !self.mark(tree, ObjectType::Tree) {
            return Ok(());
        }
        let object = db.read_object(tree)?;
        for entry in TreeEntries::new(format, &object.body) {
            let entry = entry?;
            if entry.is_gitlink() {
                continue;
            }
            if entry.is_tree() {
                self.mark_tree_closure(db, format, &entry.oid)?;
            } else {
                self.mark(&entry.oid, ObjectType::Blob);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ObjectDatabase {
    format: ObjectFormat,
    // Behind a `Mutex` so `write_object` can take `&self` (matching the
    // `ObjectWriter` trait) and a single handle can interleave reads and writes
    // without a `&mut` borrow — the same shared-by-`&` shape the file-backed
    // database uses for its caches. Removes the need for callers to wrap this in
    // a `RefCell`/`&mut` just to write (see sley-fetch's former `RefCell` dance).
    objects: Mutex<HashMap<ObjectId, Arc<EncodedObject>>>,
    promisor: bool,
}

impl ObjectDatabase {
    pub fn new(format: ObjectFormat) -> Self {
        Self {
            format,
            objects: Mutex::new(HashMap::new()),
            promisor: false,
        }
    }

    pub fn with_promisor(mut self, promisor: bool) -> Self {
        self.promisor = promisor;
        self
    }

    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.objects
            .lock()
            .map(|objects| objects.contains_key(oid))
            .unwrap_or(false)
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
            .lock()
            .map_err(|_| GitError::object_not_found_in(*oid, MissingObjectContext::Read))?
            .get(oid)
            .map(Arc::clone)
            .or_else(|| implied_empty_tree_object(self.format, oid))
            .ok_or_else(|| GitError::object_not_found_in(*oid, MissingObjectContext::Read))
    }
}

impl ObjectWriter for ObjectDatabase {
    fn write_object(&self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        self.objects
            .lock()
            .map_err(|_| GitError::Io("object cache lock poisoned".into()))?
            .entry(oid)
            .or_insert_with(|| Arc::new(object));
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

#[cfg(feature = "mmap")]
fn load_pack_index_data(index_path: &Path) -> Result<Arc<dyn PackIndexByteSource>> {
    match sley_mmap::MappedFile::open_pack(index_path) {
        Ok(mapped) => Ok(Arc::new(mapped)),
        Err(_) => Ok(Arc::new(fs::read(index_path)?)),
    }
}

#[cfg(not(feature = "mmap"))]
fn load_pack_index_data(index_path: &Path) -> Result<Arc<dyn PackIndexByteSource>> {
    Ok(Arc::new(fs::read(index_path)?))
}

#[cfg(feature = "mmap")]
fn load_multi_pack_index_lookup_data(midx_path: &Path) -> Result<Arc<dyn PackIndexByteSource>> {
    match sley_mmap::MappedFile::open_multi_pack_index(midx_path) {
        Ok(mapped) => Ok(Arc::new(mapped)),
        Err(_) => Ok(Arc::new(fs::read(midx_path)?)),
    }
}

#[cfg(not(feature = "mmap"))]
fn load_multi_pack_index_lookup_data(midx_path: &Path) -> Result<Arc<dyn PackIndexByteSource>> {
    Ok(Arc::new(fs::read(midx_path)?))
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

/// Per-pack memo of `in-pack offset -> end-of-chain object type` for the
/// `cat-file --batch-check` header fast path. Resolving a packed delta's *type*
/// walks the delta chain to its base; without this memo every header read
/// re-walks (and re-inflates) the whole chain, so reading every object in a
/// deeply-deltified pack is super-linear (sley#26). The type only depends on the
/// chain base, so memoizing `offset -> type` lets each chain be walked at most
/// once across a batch. Keyed by pack path so an offset key is never applied to
/// the wrong pack's bytes; shared across cloned handles.
/// One pack's offset-keyed header memo (see [`PackHeaderTypeCaches`]).
type PackHeaderTypeCache = Arc<Mutex<HashMap<u64, (ObjectType, u64)>>>;

type PackHeaderTypeCaches = Arc<Mutex<HashMap<PathBuf, PackHeaderTypeCache>>>;

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
    map: HashMap<K, LruEntry<K>>,
    head: Option<K>,
    tail: Option<K>,
}

#[derive(Debug)]
struct LruEntry<K> {
    object: Arc<EncodedObject>,
    prev: Option<K>,
    next: Option<K>,
}

impl<K: std::hash::Hash + Eq + Clone> LruCache<K> {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            map: HashMap::new(),
            head: None,
            tail: None,
        }
    }

    fn get(&mut self, key: &K) -> Option<Arc<EncodedObject>> {
        let object = Arc::clone(&self.map.get(key)?.object);
        self.touch(key);
        Some(object)
    }

    /// Move `key` to the most-recently-used end in O(1).
    fn touch(&mut self, key: &K) {
        if self.tail.as_ref() == Some(key) {
            return;
        }
        if self.map.contains_key(key) {
            self.detach(key);
            self.attach_back(key.clone());
        }
    }

    /// Drop `key` from both the map and the recency queue, releasing its budget.
    fn remove(&mut self, key: &K) {
        if let Some(entry) = self.map.get(key) {
            self.used = self.used.saturating_sub(cached_object_cost(&entry.object));
        }
        self.detach(key);
        self.map.remove(key);
    }

    fn detach(&mut self, key: &K) {
        let Some((prev, next)) = self.map.get_mut(key).map(|entry| {
            let prev = entry.prev.take();
            let next = entry.next.take();
            (prev, next)
        }) else {
            return;
        };

        match &prev {
            Some(prev_key) => {
                if let Some(prev_entry) = self.map.get_mut(prev_key) {
                    prev_entry.next = next.clone();
                }
            }
            None => self.head = next.clone(),
        }
        match &next {
            Some(next_key) => {
                if let Some(next_entry) = self.map.get_mut(next_key) {
                    next_entry.prev = prev.clone();
                }
            }
            None => self.tail = prev.clone(),
        }
    }

    fn attach_back(&mut self, key: K) {
        let previous_tail = self.tail.replace(key.clone());
        match previous_tail {
            Some(tail_key) => {
                if let Some(tail_entry) = self.map.get_mut(&tail_key) {
                    tail_entry.next = Some(key.clone());
                }
                if let Some(entry) = self.map.get_mut(&key) {
                    entry.prev = Some(tail_key);
                    entry.next = None;
                }
            }
            None => {
                self.head = Some(key.clone());
                if let Some(entry) = self.map.get_mut(&key) {
                    entry.prev = None;
                    entry.next = None;
                }
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.head = None;
        self.tail = None;
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
        if let Some(entry) = self.map.get_mut(&key) {
            let previous = std::mem::replace(&mut entry.object, object);
            // Replacing an existing entry: adjust accounting and refresh recency.
            self.used = self
                .used
                .saturating_sub(cached_object_cost(&previous))
                .saturating_add(cost);
            self.touch(&key);
        } else {
            self.used = self.used.saturating_add(cost);
            self.map.insert(
                key.clone(),
                LruEntry {
                    object,
                    prev: None,
                    next: None,
                },
            );
            self.attach_back(key);
        }
        while self.used > self.budget {
            let Some(evicted) = self.head.clone() else {
                break;
            };
            self.remove(&evicted);
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

/// Bridges a per-pack `offset -> ObjectType` memo into the header fast path so
/// the ofs-delta chain walk is performed at most once per chain across a batch
/// of `read_object_header` calls (sley#26).
struct PackHeaderTypeCacheAdapter<'a>(&'a PackHeaderTypeCache);

impl sley_pack::HeaderTypeCache for PackHeaderTypeCacheAdapter<'_> {
    fn get(&self, pack_offset: u64) -> Option<(ObjectType, u64)> {
        self.0.lock().ok()?.get(&pack_offset).copied()
    }

    fn put(&mut self, pack_offset: u64, header: (ObjectType, u64)) {
        if let Ok(mut cache) = self.0.lock() {
            cache.insert(pack_offset, header);
        }
    }
}

/// Parsed pack indexes keyed by `.idx` path, shared across cloned handles. This
/// remains for MIDX and path-only fallback lookups; normal pack-directory scans
/// use [`PackRegistrySnapshot`] so the lookup hot path can walk already-parsed
/// pack records directly.
type PackIndexCache = Arc<Mutex<HashMap<PathBuf, Arc<PackIndex>>>>;

/// Parsed multi-pack-index files keyed by path, shared across cloned handles.
/// Caches the MIDX parse so object lookups in repositories with a MIDX avoid
/// reparsing the same fanout/object tables for every read.
type MultiPackIndexCache = Arc<Mutex<HashMap<PathBuf, Arc<MultiPackIndex>>>>;

/// Raw multi-pack-index OID lookup tables keyed by path, shared across cloned
/// handles. These avoid hashing and materializing every MIDX object when a
/// command only needs point lookups.
type MultiPackIndexOidLookupCache = Arc<Mutex<HashMap<PathBuf, Arc<MultiPackIndexOidLookup>>>>;

/// One registered `.idx`/`.pack` pair from a pack directory. The index is parsed
/// when the registry snapshot is built; pack bytes and per-pack decode/header
/// caches hang directly off this record so repeated object lookups do not bounce
/// through path-keyed maps.
#[derive(Debug)]
struct RegisteredPack {
    idx: PathBuf,
    pack: PathBuf,
    index: Mutex<Option<Arc<PackIndexViewData>>>,
    data: Mutex<Option<Arc<PackData>>>,
    delta_cache: Arc<Mutex<LruOffsetCache>>,
    header_type_cache: PackHeaderTypeCache,
}

impl RegisteredPack {
    fn new(idx: PathBuf, pack: PathBuf) -> Self {
        Self {
            idx,
            pack,
            index: Mutex::new(None),
            data: Mutex::new(None),
            delta_cache: Arc::new(Mutex::new(LruOffsetCache::new(delta_base_cache_budget()))),
            header_type_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn index(&self, format: ObjectFormat) -> Result<Arc<PackIndexViewData>> {
        if let Ok(cache) = self.index.lock()
            && let Some(index) = cache.as_ref()
        {
            return Ok(Arc::clone(index));
        }
        let index_bytes = load_pack_index_data(&self.idx)?;
        let index = Arc::new(PackIndexViewData::parse_trusted_source_without_checksum(
            index_bytes,
            format,
        )?);
        if let Ok(mut cache) = self.index.lock() {
            *cache = Some(Arc::clone(&index));
        }
        Ok(index)
    }

    fn bytes(&self, pack_bytes: &PackBytesCache) -> Result<Arc<PackData>> {
        if let Ok(cache) = self.data.lock()
            && let Some(bytes) = cache.as_ref()
        {
            return Ok(Arc::clone(bytes));
        }
        if let Ok(cache) = pack_bytes.lock()
            && let Some(bytes) = cache.get(&self.pack)
        {
            let bytes = Arc::clone(bytes);
            if let Ok(mut local_cache) = self.data.lock() {
                *local_cache = Some(Arc::clone(&bytes));
            }
            return Ok(bytes);
        }
        let bytes = Arc::new(load_pack_data(&self.pack)?);
        if let Ok(mut local_cache) = self.data.lock() {
            *local_cache = Some(Arc::clone(&bytes));
        }
        if let Ok(mut cache) = pack_bytes.lock() {
            cache.insert(self.pack.clone(), Arc::clone(&bytes));
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackDirFingerprint {
    modified: Option<std::time::SystemTime>,
    idx_count: usize,
    pack_count: usize,
}

/// Snapshot of a pack directory's lookup state, shared across cloned handles.
/// New packs are still found: a lookup that misses every cached pack re-scans the
/// directory once before concluding the object is absent (see
/// [`FileObjectDatabase::find_pack_containing`]).
#[derive(Debug)]
struct PackRegistrySnapshot {
    fingerprint: PackDirFingerprint,
    packs: Vec<Arc<RegisteredPack>>,
    recent_pack: Mutex<Option<usize>>,
}

impl PackRegistrySnapshot {
    fn new(fingerprint: PackDirFingerprint, packs: Vec<Arc<RegisteredPack>>) -> Self {
        Self {
            fingerprint,
            packs,
            recent_pack: Mutex::new(None),
        }
    }

    fn cached_hint(&self) -> Option<usize> {
        self.recent_pack
            .lock()
            .ok()
            .and_then(|hint| *hint)
            .filter(|pack_index| *pack_index < self.packs.len())
    }

    fn remember_hint(&self, pack_index: usize) {
        if let Ok(mut hint) = self.recent_pack.lock() {
            *hint = Some(pack_index);
        }
    }
}

/// Cached pack-registry snapshot for this object directory, shared across cloned
/// handles. A `FileObjectDatabase` owns exactly one object directory, so this is
/// an `Option` instead of another path-keyed map.
type PackRegistryCache = Arc<Mutex<Option<Arc<PackRegistrySnapshot>>>>;

#[derive(Debug, Clone)]
struct PackLookup {
    pack: PathBuf,
    registered: Option<Arc<RegisteredPack>>,
    offset: u64,
}

impl PackLookup {
    fn from_registered(pack: Arc<RegisteredPack>, offset: u64) -> Self {
        Self {
            pack: pack.pack.clone(),
            registered: Some(pack),
            offset,
        }
    }

    fn from_path(pack: PathBuf, offset: u64) -> Self {
        Self {
            pack,
            registered: None,
            offset,
        }
    }

    fn pack_path(&self) -> &Path {
        &self.pack
    }

    fn pack_bytes(&self, database: &FileObjectDatabase) -> Result<Arc<PackData>> {
        match &self.registered {
            Some(pack) => pack.bytes(&database.pack_bytes),
            None => database.cached_pack_bytes(&self.pack),
        }
    }

    fn pack_index(&self, database: &FileObjectDatabase) -> Result<Arc<PackIndex>> {
        match &self.registered {
            Some(pack) => database.cached_pack_index(&pack.idx),
            None => database.cached_pack_index(&self.pack.with_extension("idx")),
        }
    }

    fn delta_cache(&self, database: &FileObjectDatabase) -> Option<Arc<Mutex<LruOffsetCache>>> {
        match &self.registered {
            Some(pack) => Some(Arc::clone(&pack.delta_cache)),
            None => database.pack_delta_cache(&self.pack),
        }
    }

    fn header_type_cache(&self, database: &FileObjectDatabase) -> Option<PackHeaderTypeCache> {
        match &self.registered {
            Some(pack) => Some(Arc::clone(&pack.header_type_cache)),
            None => database.pack_header_type_cache(&self.pack),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileObjectDatabase {
    loose: LooseObjectStore,
    objects_dir: PathBuf,
    alternates: Vec<PathBuf>,
    format: ObjectFormat,
    pack_bytes: PackBytesCache,
    pack_indexes: PackIndexCache,
    multi_pack_indexes: MultiPackIndexCache,
    multi_pack_oid_lookups: MultiPackIndexOidLookupCache,
    pack_registry: PackRegistryCache,
    decoded: DecodedObjectCache,
    pack_deltas: PackDeltaCaches,
    pack_header_types: PackHeaderTypeCaches,
    promisor_objects: Arc<OnceLock<HashSet<ObjectId>>>,
    /// Whether the owning repository actually has a promisor remote configured
    /// (`extensions.partialclone` is set, or some `remote.<name>.promisor` is
    /// true). Mirrors git's `is_promisor_object`, which only treats objects in
    /// `.promisor` packs as "promised" when `repo_has_promisor_remote()` holds:
    /// a stray `.promisor` sidecar in a non-partial repo must NOT excuse missing
    /// objects from fsck. Defaults to `false`; the fsck driver opts in after
    /// reading the repo config.
    promisor_remote_present: bool,
    /// Graft points (`$GIT_DIR/shallow`), loaded lazily on the first
    /// [`ObjectReader::is_shallow_graft`] query. `$GIT_DIR` is taken to be
    /// the parent of `objects_dir`, matching the standard layout.
    shallow_grafts: Arc<std::sync::OnceLock<HashSet<ObjectId>>>,
}

#[derive(Debug)]
pub struct ObjectPresenceChecker {
    db: FileObjectDatabase,
    pack_dir: PathBuf,
    midx: Option<Arc<MultiPackIndexOidLookup>>,
    registry: Option<Arc<PackRegistrySnapshot>>,
    registry_indexes: Vec<Option<Arc<PackIndexViewData>>>,
    recent_pack: Option<usize>,
    prepared_packs: bool,
    prepared_registry: bool,
}

impl ObjectPresenceChecker {
    fn new(db: FileObjectDatabase) -> Self {
        let pack_dir = db.objects_dir.join("pack");
        Self {
            db,
            pack_dir,
            midx: None,
            registry: None,
            registry_indexes: Vec::new(),
            recent_pack: None,
            prepared_packs: false,
            prepared_registry: false,
        }
    }

    pub fn contains(&mut self, oid: &ObjectId) -> Result<bool> {
        if oid.format() != self.db.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.db.format.name()
            )));
        }
        if self.db.loose.exists(oid)? {
            return Ok(true);
        }
        if self.find_packed(oid, false)? {
            return Ok(true);
        }
        if self.find_packed(oid, true)? {
            return Ok(true);
        }
        for alternate in &self.db.alternates {
            if FileObjectDatabase::without_alternates(alternate, self.db.format).contains(oid)? {
                return Ok(true);
            }
        }
        // Preserve the regular contains() reprepare-on-miss behavior for loose
        // objects that appeared after the fanout cache was populated.
        self.db.loose.invalidate_cache();
        self.db.loose.exists(oid)
    }

    fn find_packed(&mut self, oid: &ObjectId, force_rescan: bool) -> Result<bool> {
        self.prepare_packs(force_rescan)?;
        if let Some(midx) = &self.midx
            && midx.contains(oid)
        {
            return Ok(true);
        }
        self.prepare_registry(force_rescan)?;
        self.find_in_registry(oid)
    }

    fn prepare_packs(&mut self, force_rescan: bool) -> Result<()> {
        if self.prepared_packs && !force_rescan {
            return Ok(());
        }
        let midx_path = self.pack_dir.join("multi-pack-index");
        self.midx = self.db.cached_multi_pack_index_oid_lookup(&midx_path)?;
        self.prepared_packs = true;
        Ok(())
    }

    fn prepare_registry(&mut self, force_rescan: bool) -> Result<()> {
        if self.prepared_registry && !force_rescan {
            return Ok(());
        }
        let registry = self.db.cached_pack_registry(&self.pack_dir, force_rescan)?;
        let registry_changed = match self.registry.as_ref() {
            Some(cached) => !Arc::ptr_eq(cached, &registry),
            None => true,
        };
        if registry_changed {
            self.registry_indexes = vec![None; registry.packs.len()];
            self.recent_pack = None;
            self.registry = Some(registry);
        }
        self.prepared_registry = true;
        Ok(())
    }

    fn find_in_registry(&mut self, oid: &ObjectId) -> Result<bool> {
        let Some(registry) = self.registry.as_ref().map(Arc::clone) else {
            return Ok(false);
        };
        if let Some(pack_index) = self
            .recent_pack
            .filter(|pack_index| *pack_index < registry.packs.len())
        {
            let index = self.registry_index(&registry, pack_index)?;
            if index.find(oid).is_some() {
                return Ok(true);
            }
        }
        for pack_index in 0..registry.packs.len() {
            if Some(pack_index) == self.recent_pack {
                continue;
            }
            let index = self.registry_index(&registry, pack_index)?;
            if index.find(oid).is_some() {
                self.recent_pack = Some(pack_index);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn registry_index(
        &mut self,
        registry: &PackRegistrySnapshot,
        pack_index: usize,
    ) -> Result<Arc<PackIndexViewData>> {
        if self.registry_indexes.len() != registry.packs.len() {
            self.registry_indexes = vec![None; registry.packs.len()];
            self.recent_pack = None;
        }
        if let Some(index) = self
            .registry_indexes
            .get(pack_index)
            .and_then(|index| index.as_ref())
        {
            return Ok(Arc::clone(index));
        }
        let index = registry.packs[pack_index].index(self.db.format)?;
        if let Some(slot) = self.registry_indexes.get_mut(pack_index) {
            *slot = Some(Arc::clone(&index));
        }
        Ok(index)
    }
}

/// Parse `$GIT_DIR/shallow`: one hex object id per line. A missing file is an
/// empty set (the repository is not shallow); unparsable lines are ignored so
/// a torn write never poisons walks.
fn read_shallow_grafts(shallow_file: &Path, format: ObjectFormat) -> HashSet<ObjectId> {
    let Ok(contents) = std::fs::read_to_string(shallow_file) else {
        return HashSet::new();
    };
    contents
        .lines()
        .filter_map(|line| ObjectId::from_hex(format, line.trim()).ok())
        .collect()
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

fn collect_loose_fanout_object_ids(
    objects_dir: &Path,
    format: ObjectFormat,
    fanout: u8,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    let fanout_hex = format!("{fanout:02x}");
    let fanout_dir = objects_dir.join(&fanout_hex);
    let entries = match fs::read_dir(&fanout_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let hex_len = format.hex_len();
    for object_entry in entries {
        let object_entry = object_entry?;
        let name = object_entry.file_name();
        let Some(suffix) = name.to_str() else {
            continue;
        };
        if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        oids.insert(ObjectId::from_hex(
            format,
            &format!("{fanout_hex}{suffix}"),
        )?);
    }
    Ok(())
}

/// The set of `objects/XX/` fanout directories that actually exist on disk,
/// learned from a single `read_dir(objects/)`. A freshly cloned or repacked
/// repository has zero loose-object fanout dirs (everything is packed), so this
/// lets a loose-presence probe skip the per-fanout `opendir(objects/XX)` that
/// would otherwise miss with ENOENT on every distinct id prefix — the
/// constant-factor loose-probe floor on packed-repo reads. Returns the present
/// fanout bytes (`0x00..=0xff`); a missing `objects/` dir yields the empty set.
fn present_loose_fanouts(objects_dir: &Path) -> Result<HashSet<u8>> {
    let mut present = HashSet::new();
    let entries = match fs::read_dir(objects_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(present),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() != 2 {
            continue;
        }
        let mut bytes = name.bytes();
        let (Some(hi), Some(lo)) = (bytes.next(), bytes.next()) else {
            continue;
        };
        let (Some(hi), Some(lo)) = ((hi as char).to_digit(16), (lo as char).to_digit(16)) else {
            continue;
        };
        // Only count it as a fanout dir if it really is a directory; `git` keeps
        // non-fanout entries (`pack`, `info`) under `objects/` that happen to be
        // dirs too, but those never collide with a two-hex-char name.
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            present.insert(((hi << 4) | lo) as u8);
        }
    }
    Ok(present)
}

#[derive(Debug, Default)]
struct LoosePresenceCache {
    /// Fanout bytes whose `objects/XX/` listing has been folded into `objects`.
    loaded_fanouts: HashSet<u8>,
    objects: HashSet<ObjectId>,
    /// Which of the 256 `objects/XX/` fanout dirs exist on disk, learned from a
    /// single `read_dir(objects/)`. `None` until first queried. A fanout absent
    /// from this set cannot hold a loose object, so its per-fanout `read_dir`
    /// (which would miss with ENOENT) is skipped entirely.
    present_fanouts: Option<HashSet<u8>>,
}

/// Every object id resolvable through a pack (any `.idx` or the
/// multi-pack-index) under `objects_dir/pack`. Used by `--unpacked`
/// filtering: an object is "unpacked" when absent from this set, regardless
/// of a loose copy also existing.
pub fn packed_object_ids(
    objects_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<HashSet<ObjectId>> {
    let mut oids = HashSet::new();
    collect_packed_object_ids(&objects_dir.as_ref().join("pack"), format, &mut oids)?;
    Ok(oids)
}

pub fn kept_pack_object_ids(
    objects_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<HashSet<ObjectId>> {
    let pack_dir = objects_dir.as_ref().join("pack");
    let mut oids = HashSet::new();
    if !pack_dir.exists() {
        return Ok(oids);
    }
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        if !path.with_extension("pack").exists() || !path.with_extension("keep").exists() {
            continue;
        }
        let index = PackIndex::parse(&fs::read(path)?, format)?;
        oids.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(oids)
}

fn collect_packed_object_ids(
    pack_dir: &Path,
    format: ObjectFormat,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !pack_dir.exists() {
        return Ok(());
    }
    let mut midx_pack_names = HashSet::new();
    let midx_path = pack_dir.join("multi-pack-index");
    if midx_path.exists() {
        let midx = MultiPackIndex::parse_without_checksum(&fs::read(&midx_path)?, format)?;
        midx_pack_names.extend(midx.pack_names.iter().cloned());
        oids.extend(midx.objects.into_iter().map(|entry| entry.oid));
    }
    collect_incremental_midx_object_ids(pack_dir, format, oids)?;
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        if !path.with_extension("pack").exists() {
            continue;
        }
        let index = match PackIndex::parse(&fs::read(&path)?, format) {
            Ok(index) => index,
            Err(_err)
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| midx_pack_names.contains(name)) =>
            {
                eprintln!(
                    "error: packfile {} index unavailable",
                    path.with_extension("pack").display()
                );
                continue;
            }
            Err(err) => return Err(err),
        };
        oids.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(())
}

fn read_incremental_midx_chain(pack_dir: &Path) -> Result<Vec<String>> {
    let path = pack_dir
        .join("multi-pack-index.d")
        .join("multi-pack-index-chain");
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn collect_incremental_midx_object_ids(
    pack_dir: &Path,
    format: ObjectFormat,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    let chain = read_incremental_midx_chain(pack_dir)?;
    if chain.is_empty() {
        return Ok(());
    }
    let midx_dir = pack_dir.join("multi-pack-index.d");
    for checksum in chain {
        let path = midx_dir.join(format!("multi-pack-index-{checksum}.midx"));
        let midx = MultiPackIndex::parse_without_checksum(&fs::read(path)?, format)?;
        oids.extend(midx.objects.into_iter().map(|entry| entry.oid));
    }
    Ok(())
}

impl FileObjectDatabase {
    /// The object-id format (hash algorithm) this database was opened with.
    pub fn object_format(&self) -> ObjectFormat {
        self.format
    }

    /// The repository object directory this database reads from.
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

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
            multi_pack_oid_lookups: Arc::new(Mutex::new(HashMap::new())),
            pack_registry: Arc::new(Mutex::new(None)),
            decoded: Arc::new(Mutex::new(LruObjectCache::new(object_cache_budget()))),
            pack_deltas: Arc::new(Mutex::new(HashMap::new())),
            pack_header_types: Arc::new(Mutex::new(HashMap::new())),
            promisor_objects: Arc::new(OnceLock::new()),
            promisor_remote_present: false,
            shallow_grafts: Arc::new(std::sync::OnceLock::new()),
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
            multi_pack_oid_lookups: Arc::new(Mutex::new(HashMap::new())),
            pack_registry: Arc::new(Mutex::new(None)),
            decoded: Arc::new(Mutex::new(LruObjectCache::new(object_cache_budget()))),
            pack_deltas: Arc::new(Mutex::new(HashMap::new())),
            pack_header_types: Arc::new(Mutex::new(HashMap::new())),
            promisor_objects: Arc::new(OnceLock::new()),
            promisor_remote_present: false,
            shallow_grafts: Arc::new(std::sync::OnceLock::new()),
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        Self::new(repository_objects_dir(git_dir), format)
    }

    /// Declare whether the owning repository has a promisor remote configured.
    /// Only when this holds does [`ObjectReader::is_promised_object`] treat
    /// objects in `.promisor` packs (and their transitive references) as
    /// promised — matching git's `is_promisor_object`, which is gated on
    /// `repo_has_promisor_remote()`. Callers that know the repo config (e.g. the
    /// fsck driver) opt in; readers built without config keep the safe default
    /// of `false`, so a stray `.promisor` sidecar never silently excuses a
    /// genuinely missing object.
    pub fn with_promisor_remote_present(mut self, present: bool) -> Self {
        self.promisor_remote_present = present;
        self
    }

    /// Drop cached pack registries, indexes, and decoded objects so the next read
    /// sees packs/objects installed after this handle was created (e.g. after
    /// `fetch` or `install_pack`). Long-lived [`Repository`] sessions call this
    /// via the owning repository's `refresh_objects` hook.
    pub fn refresh_read_cache(&self) {
        if let Ok(mut cache) = self.pack_registry.lock() {
            *cache = None;
        }
        if let Ok(mut cache) = self.pack_indexes.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.multi_pack_indexes.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.multi_pack_oid_lookups.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.pack_bytes.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.pack_deltas.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.pack_header_types.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.decoded.lock() {
            cache.clear();
        }
        self.loose.invalidate_cache();
    }

    pub fn loose(&self) -> &LooseObjectStore {
        &self.loose
    }

    pub fn presence_checker(&self) -> ObjectPresenceChecker {
        ObjectPresenceChecker::new(self.clone())
    }

    pub fn install_pack(&self, pack: &PackWrite) -> Result<PackInstallResult> {
        self.install_pack_with_options(pack, RawPackInstallOptions::default())
    }

    pub fn write_blob_as_pack(
        &self,
        oid: ObjectId,
        object: &EncodedObject,
        compression_level: u32,
    ) -> Result<ObjectId> {
        if object.object_type != ObjectType::Blob {
            return Err(GitError::InvalidObject(
                "write_blob_as_pack requires a blob object".into(),
            ));
        }
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        if self.contains(&oid)? {
            return Ok(oid);
        }
        let input = [PackInput { oid: &oid, object }];
        let options = PackWriteOptions::new()
            .with_window(0)
            .with_depth(0)
            .with_reorder(false)
            .with_compression_level(compression_level);
        let pack =
            PackFile::write_packed_with_known_ids_and_options(&input, self.format, &options)?;
        self.install_pack(&pack)?;
        Ok(oid)
    }

    pub fn write_blobs_as_pack(
        &self,
        objects: &[(ObjectId, EncodedObject)],
        compression_level: u32,
    ) -> Result<()> {
        let mut seen = HashSet::with_capacity(objects.len());
        let mut inputs = Vec::new();
        for (oid, object) in objects {
            if object.object_type != ObjectType::Blob {
                return Err(GitError::InvalidObject(
                    "write_blobs_as_pack requires blob objects".into(),
                ));
            }
            if oid.format() != self.format {
                return Err(GitError::InvalidObjectId(format!(
                    "object {oid} uses {}, store uses {}",
                    oid.format().name(),
                    self.format.name()
                )));
            }
            if seen.insert(*oid) && !self.contains(oid)? {
                inputs.push(PackInput { oid, object });
            }
        }
        if inputs.is_empty() {
            return Ok(());
        }
        let options = PackWriteOptions::new()
            .with_window(0)
            .with_depth(0)
            .with_reorder(false)
            .with_compression_level(compression_level);
        let pack =
            PackFile::write_packed_with_known_ids_and_options(&inputs, self.format, &options)?;
        self.install_pack(&pack)?;
        Ok(())
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

    fn install_pack_file_from_temp(
        &self,
        temp_pack_path: &Path,
        pack_checksum: ObjectId,
        index: &[u8],
        object_ids: Vec<ObjectId>,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", pack_checksum.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        match fs::rename(temp_pack_path, &pack_path) {
            Ok(()) => {}
            Err(_) if pack_path.exists() => {
                let _ = fs::remove_file(temp_pack_path);
            }
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
        write_pack_component(&index_path, index)?;
        let promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, options.promisor)?;
        Ok(PackInstallResult {
            pack_name,
            pack_path,
            index_path,
            promisor_path,
            object_ids,
        })
    }

    pub fn install_raw_pack_from_reader<R>(&self, reader: &mut R) -> Result<PackInstallResult>
    where
        R: Read,
    {
        self.install_raw_pack_from_reader_with_options(reader, RawPackInstallOptions::default())
    }

    pub fn begin_raw_pack_install(
        &self,
        expected_pack_id: ObjectId,
        expected_pack_size: u64,
    ) -> Result<RawPackStreamingInstall> {
        self.begin_raw_pack_install_with_options(
            expected_pack_id,
            expected_pack_size,
            RawPackInstallOptions::default(),
        )
    }

    pub fn begin_raw_pack_install_with_options(
        &self,
        expected_pack_id: ObjectId,
        expected_pack_size: u64,
        options: RawPackInstallOptions,
    ) -> Result<RawPackStreamingInstall> {
        if expected_pack_id.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "pack checksum uses {}, store uses {}",
                expected_pack_id.format().name(),
                self.format.name()
            )));
        }
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", expected_pack_id.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        let temp_pack_path = unique_temp_path(&pack_dir);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_pack_path)?;
        Ok(RawPackStreamingInstall {
            format: self.format,
            expected_pack_id,
            expected_pack_size,
            options,
            pack_dir,
            pack_name,
            pack_path,
            index_path,
            temp_pack_path,
            file: Some(file),
            written: 0,
            finished: false,
        })
    }

    pub fn install_raw_pack_from_reader_with_options<R>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult>
    where
        R: Read,
    {
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let temp_pack_path = unique_temp_path(&pack_dir);
        let result = (|| -> Result<PackInstallResult> {
            // Stage directly in objects/pack so validation, indexing, and the
            // eventual checksum-named rename use one streamed write.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_pack_path)?;
            let built = {
                let mut tee = PackInstallTeeReader {
                    reader,
                    writer: &mut file,
                };
                PackIndex::write_v2_for_pack_reader_to_trailer(&mut tee, self.format)?
            };
            file.flush()?;
            file.sync_all()?;
            drop(file);

            self.install_pack_file_from_temp(
                &temp_pack_path,
                built.pack_checksum,
                &built.index,
                built.entries.iter().map(|entry| entry.oid).collect(),
                options,
            )
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_pack_path);
        }
        result
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
        // Reprepare-on-miss: a cached negative loose verdict may predate a
        // sibling write. Drop it and exact-probe once before reporting absence.
        self.loose.invalidate_cache();
        self.loose.exists(oid)
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
        // Reprepare-on-miss: drop any stale negative loose cache and exact-probe
        // once before reporting absence (see `read_object`).
        self.loose.invalidate_cache();
        if let Some(disk_size) = self.loose.disk_size(oid)? {
            return Ok(Some(ObjectStorageInfo {
                disk_size,
                deltabase: zero_oid(self.format)?,
            }));
        }
        Ok(None)
    }

    pub fn resolve_prefix(&self, prefix: &str) -> Result<ObjectPrefixResolution> {
        let mut matches = self.object_ids_with_prefix(prefix)?;
        Ok(match matches.len() {
            0 => ObjectPrefixResolution::Missing,
            1 => ObjectPrefixResolution::Unique(matches.remove(0)),
            _ => ObjectPrefixResolution::Ambiguous(matches),
        })
    }

    pub fn object_ids_with_prefix(&self, prefix: &str) -> Result<Vec<ObjectId>> {
        validate_object_id_prefix(self.format, prefix)?;
        let mut matches = Vec::new();
        for oid in self.object_ids()? {
            if object_id_matches_prefix(&oid, prefix) {
                matches.push(oid);
            }
        }
        Ok(matches)
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
        if implied_empty_tree_object(self.format, oid).is_some() {
            return Ok(Some((ObjectType::Tree, 0)));
        }
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(oid)
        {
            return Ok(Some((object.object_type, object.body.len() as u64)));
        }
        if let Some(header) = self.loose.read_header(oid)? {
            return Ok(Some(header));
        }
        if let Some(pack_lookup) = self.find_pack_containing(oid)? {
            let bytes = pack_lookup.pack_bytes(self)?;
            // Per-pack offset->type memo so the ofs-delta chain walk that resolves
            // a packed object's type runs at most once per chain across the batch,
            // instead of re-walking (and re-inflating each link's leading varints)
            // on every header read — the sley#26 super-linear cat-file --batch-check.
            let type_cache = pack_lookup.header_type_cache(self);
            let resolve_ref_base = |base: &ObjectId| {
                self.read_object_header(base)
                    .map(|header| header.map(|(t, _)| t))
            };
            let header = match &type_cache {
                Some(cache) => {
                    let mut adapter = PackHeaderTypeCacheAdapter(cache);
                    sley_pack::read_object_header_at_with_cache(
                        &bytes,
                        pack_lookup.offset,
                        self.format,
                        resolve_ref_base,
                        &mut adapter,
                    )?
                }
                None => sley_pack::read_object_header_at(
                    &bytes,
                    pack_lookup.offset,
                    self.format,
                    resolve_ref_base,
                )?,
            };
            return Ok(Some(header));
        }
        for alternate in &self.alternates {
            if let Some(header) =
                Self::without_alternates(alternate, self.format).read_object_header(oid)?
            {
                return Ok(Some(header));
            }
        }
        // Reprepare-on-miss: discard any stale negative loose cache and retry an
        // exact path probe once before reporting absence (see `read_object`).
        self.loose.invalidate_cache();
        if let Some(header) = self.loose.read_header(oid)? {
            return Ok(Some(header));
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
        let Some(pack_lookup) = self.find_pack_containing(oid)? else {
            return Ok(None);
        };
        self.read_packed_object_at_lookup(oid, &pack_lookup)
            .map(Some)
    }

    fn read_packed_object_at_lookup(
        &self,
        oid: &ObjectId,
        pack_lookup: &PackLookup,
    ) -> Result<Arc<EncodedObject>> {
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(oid)
        {
            return Ok(object);
        }
        let bytes = pack_lookup.pack_bytes(self)?;
        // Per-pack delta-base cache (keyed by in-pack offset). Resolving an
        // ofs-delta chain reuses already-decoded bases instead of re-inflating the
        // whole chain on every read. Scoped to this pack's path so an offset key is
        // never applied to the wrong pack's bytes.
        let delta_cache = pack_lookup.delta_cache(self);
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
                pack_lookup.offset,
                self.format,
                resolve_ref_base,
                adapter,
            )?,
            None => sley_pack::read_object_at_arc(
                &bytes,
                pack_lookup.offset,
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
        Ok(object)
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

    /// The per-pack header-type memo for `pack_path`, creating it on first use.
    /// Returns `None` only if the shared map's lock is poisoned, in which case the
    /// caller falls back to an unmemoized header walk (correctness preserved).
    fn pack_header_type_cache(&self, pack_path: &Path) -> Option<PackHeaderTypeCache> {
        let mut caches = self.pack_header_types.lock().ok()?;
        let cache = caches
            .entry(pack_path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(HashMap::new())));
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

    fn cached_multi_pack_index_oid_lookup(
        &self,
        midx_path: &Path,
    ) -> Result<Option<Arc<MultiPackIndexOidLookup>>> {
        if !midx_path.exists() {
            return Ok(None);
        }
        if let Ok(cache) = self.multi_pack_oid_lookups.lock()
            && let Some(midx) = cache.get(midx_path)
        {
            return Ok(Some(Arc::clone(midx)));
        }
        let bytes = load_multi_pack_index_lookup_data(midx_path)?;
        let midx = match MultiPackIndexOidLookup::parse(bytes, self.format) {
            Ok(midx) => Arc::new(midx),
            Err(GitError::InvalidFormat(message))
                if message.starts_with("multi-pack-index hash id ") =>
            {
                let actual = message
                    .strip_prefix("multi-pack-index hash id ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or("0");
                let expected = match self.format {
                    ObjectFormat::Sha1 => 1,
                    ObjectFormat::Sha256 => 2,
                };
                eprintln!(
                    "error: multi-pack-index hash version {actual} does not match version {expected}"
                );
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        if let Ok(mut cache) = self.multi_pack_oid_lookups.lock() {
            cache.insert(midx_path.to_path_buf(), Arc::clone(&midx));
        }
        Ok(Some(midx))
    }

    fn cached_multi_pack_index(&self, midx_path: &Path) -> Result<Option<Arc<MultiPackIndex>>> {
        if !midx_path.exists() {
            return Ok(None);
        }
        if let Ok(cache) = self.multi_pack_indexes.lock()
            && let Some(midx) = cache.get(midx_path)
        {
            return Ok(Some(Arc::clone(midx)));
        }
        let bytes = load_multi_pack_index_lookup_data(midx_path)?;
        let midx = match MultiPackIndex::parse(bytes.as_bytes(), self.format) {
            Ok(midx) => Arc::new(midx),
            Err(GitError::InvalidFormat(message))
                if message.starts_with("multi-pack-index hash id ") =>
            {
                let actual = message
                    .strip_prefix("multi-pack-index hash id ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or("0");
                let expected = match self.format {
                    ObjectFormat::Sha1 => 1,
                    ObjectFormat::Sha256 => 2,
                };
                eprintln!(
                    "error: multi-pack-index hash version {actual} does not match version {expected}"
                );
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        if let Ok(mut cache) = self.multi_pack_indexes.lock() {
            cache.insert(midx_path.to_path_buf(), Arc::clone(&midx));
        }
        Ok(Some(midx))
    }

    /// Registry snapshot for this database's pack directory. With `force_rescan`,
    /// the directory is re-read; when the fingerprint and pack set match the
    /// cached snapshot, the same `Arc` is returned so miss handling can tell that
    /// no new packs appeared.
    fn cached_pack_registry(
        &self,
        pack_dir: &Path,
        force_rescan: bool,
    ) -> Result<Arc<PackRegistrySnapshot>> {
        if !force_rescan && let Some(registry) = self.cached_loaded_pack_registry(pack_dir)? {
            return Ok(registry);
        }
        let scanned = Arc::new(scan_pack_registry(pack_dir, self.format)?);
        if let Ok(mut cache) = self.pack_registry.lock() {
            match cache.as_ref() {
                Some(existing)
                    if existing.fingerprint == scanned.fingerprint
                        && same_registered_pack_set(&existing.packs, &scanned.packs) =>
                {
                    return Ok(Arc::clone(existing));
                }
                _ => {
                    *cache = Some(Arc::clone(&scanned));
                }
            }
        }
        Ok(scanned)
    }

    fn find_in_pack_registry(
        &self,
        registry: Arc<PackRegistrySnapshot>,
        oid: &ObjectId,
    ) -> Result<Option<PackLookup>> {
        let hinted_pack_index = registry.cached_hint();
        if let Some(pack_index) = hinted_pack_index {
            let pack = &registry.packs[pack_index];
            match pack.index(self.format) {
                Ok(index) => {
                    if let Some(entry) = index.find(oid) {
                        return Ok(Some(PackLookup::from_registered(
                            Arc::clone(pack),
                            entry.offset,
                        )));
                    }
                }
                Err(_) => {
                    eprintln!("error: packfile {} index unavailable", pack.pack.display());
                }
            }
        }
        for (pack_index, pack) in registry.packs.iter().enumerate() {
            if Some(pack_index) == hinted_pack_index {
                continue;
            }
            let index = match pack.index(self.format) {
                Ok(index) => index,
                Err(_) => {
                    eprintln!("error: packfile {} index unavailable", pack.pack.display());
                    continue;
                }
            };
            if let Some(entry) = index.find(oid) {
                registry.remember_hint(pack_index);
                return Ok(Some(PackLookup::from_registered(
                    Arc::clone(pack),
                    entry.offset,
                )));
            }
        }
        Ok(None)
    }

    /// Read `oid` from any pack *other than* the one named by `exclude`, used as
    /// a corruption fallback: a redundant packed copy survives one pack's
    /// damage. Scans the on-disk `.idx` files directly (bypassing the registry
    /// cache, whose first hit is the excluded pack) and decodes from the first
    /// other pack that both indexes the object and parses cleanly.
    fn read_packed_object_from_other_packs(
        &self,
        oid: &ObjectId,
        exclude: &PackLookup,
    ) -> Result<Option<Arc<EncodedObject>>> {
        let pack_dir = self.objects_dir.join("pack");
        let Ok(entries) = fs::read_dir(&pack_dir) else {
            return Ok(None);
        };
        let excluded_pack = exclude.pack_path().to_path_buf();
        for entry in entries {
            let idx_path = entry?.path();
            if idx_path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let pack_path = idx_path.with_extension("pack");
            if pack_path == excluded_pack {
                continue;
            }
            let Ok(idx_bytes) = fs::read(&idx_path) else {
                continue;
            };
            let Ok(index) = PackIndex::parse(&idx_bytes, self.format) else {
                continue;
            };
            let Some(entry) = index.find(oid) else {
                continue;
            };
            let candidate = PackLookup::from_path(pack_path, entry.offset);
            if let Ok(object) = self.read_packed_object_at_lookup(oid, &candidate) {
                return Ok(Some(object));
            }
        }
        Ok(None)
    }

    fn find_pack_containing(&self, oid: &ObjectId) -> Result<Option<PackLookup>> {
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        let pack_dir = self.objects_dir.join("pack");
        // Hot path: a previously cached pack registry or multi-pack-index already
        // names every pack, and locating `oid` in them is pure in-memory index
        // work. Try that first so a warm handle does not parse indexes or hash
        // pack paths on every lookup.
        if let Some(midx) = self.cached_loaded_multi_pack_index_oid_lookup()
            && let Some(pack_paths) = self.midx_oid_lookup_pack_paths(&pack_dir, &midx, oid)?
        {
            return Ok(Some(pack_paths));
        }
        if let Some(registry) = self.cached_loaded_pack_registry(&pack_dir)?
            && let Some(pack_paths) = self.find_in_pack_registry(registry, oid)?
        {
            return Ok(Some(pack_paths));
        }

        if !pack_dir.exists() {
            return Ok(None);
        }
        if let Some(pack_paths) = self.find_midx_pack_containing(&pack_dir, oid)? {
            return Ok(Some(pack_paths));
        }
        // Search the cached registry first. On a complete miss, re-scan the
        // directory once (picking up any pack added since the registry was
        // cached) and search again, so newly written packs are still found.
        let registry = self.cached_pack_registry(&pack_dir, false)?;
        if let Some(pack_paths) = self.find_in_pack_registry(Arc::clone(&registry), oid)? {
            return Ok(Some(pack_paths));
        }
        let refreshed = self.cached_pack_registry(&pack_dir, true)?;
        if Arc::ptr_eq(&registry, &refreshed) {
            // The re-scan produced the same registry, so nothing new appeared.
            return Ok(None);
        }
        self.find_in_pack_registry(refreshed, oid)
    }

    fn packed_object_storage_info(&self, oid: &ObjectId) -> Result<Option<ObjectStorageInfo>> {
        let Some(pack_lookup) = self.find_pack_containing(oid)? else {
            return Ok(None);
        };
        let index = pack_lookup.pack_index(self).ok();
        let pack = match pack_lookup.pack_bytes(self) {
            Ok(pack) => Some(pack),
            Err(_err) if index.is_some() => None,
            Err(err) => return Err(err),
        };
        let trailer_offset = pack
            .as_ref()
            .map(|pack| {
                (pack.len() as u64)
                    .checked_sub(self.format.raw_len() as u64)
                    .ok_or_else(|| {
                        GitError::InvalidFormat("pack file shorter than checksum".into())
                    })
            })
            .transpose()?;
        let delta_base = match &pack {
            Some(pack) => pack_entry_delta_base(self.format, pack, pack_lookup.offset)?,
            None => None,
        };
        let delta_base_offset = match &delta_base {
            Some(PackDeltaBase::Offset(offset)) => Some(*offset),
            Some(PackDeltaBase::Ref(_)) | None => None,
        };
        let offset_info = if let Some(index) = &index {
            scan_pack_index_offsets(index, pack_lookup.offset, trailer_offset, delta_base_offset)?
        } else if let Some(pack) = &pack {
            let end_offset =
                scan_pack_offsets_without_index(self.format, pack, pack_lookup.offset)?
                    .ok_or_else(|| {
                        GitError::InvalidFormat(format!(
                            "pack offset {} not found",
                            pack_lookup.offset
                        ))
                    })?;
            let delta_base_oid = match delta_base_offset {
                Some(offset) => self
                    .midx_oid_for_pack_offset(&pack_lookup, offset)?
                    .ok_or_else(|| {
                        GitError::InvalidFormat(format!("ofs-delta base offset {offset} not found"))
                    })?,
                None => zero_oid(self.format)?,
            };
            PackIndexOffsetInfo {
                end_offset,
                delta_base_oid: delta_base_offset.map(|_| delta_base_oid),
            }
        } else {
            return Err(GitError::InvalidFormat(
                "packed object metadata source unavailable".into(),
            ));
        };
        let disk_size = offset_info
            .end_offset
            .checked_sub(pack_lookup.offset)
            .ok_or_else(|| GitError::InvalidFormat("pack index offsets are not sorted".into()))?;
        let deltabase = match delta_base {
            Some(PackDeltaBase::Offset(_)) => offset_info.delta_base_oid.ok_or_else(|| {
                // scan_pack_index_offsets returns Err when delta_base_offset is
                // Some but no matching entry is found, so this is unreachable for
                // valid packs; propagate as an error rather than panic to keep a
                // malformed pack from taking down the process if that invariant
                // ever drifts.
                GitError::InvalidFormat("ofs-delta base oid missing from pack index".into())
            })?,
            Some(PackDeltaBase::Ref(oid)) => oid,
            None => zero_oid(self.format)?,
        };
        Ok(Some(ObjectStorageInfo {
            disk_size,
            deltabase,
        }))
    }

    fn midx_oid_for_pack_offset(
        &self,
        pack_lookup: &PackLookup,
        offset: u64,
    ) -> Result<Option<ObjectId>> {
        let pack_dir = self.objects_dir.join("pack");
        let midx_path = pack_dir.join("multi-pack-index");
        let Some(midx) = self.cached_multi_pack_index(&midx_path)? else {
            return Ok(None);
        };
        let Some(pack_name) = pack_lookup
            .pack_path()
            .file_name()
            .and_then(|name| name.to_str())
        else {
            return Ok(None);
        };
        let idx_name = pack_name
            .strip_suffix(".pack")
            .map(|stem| format!("{stem}.idx"))
            .unwrap_or_else(|| pack_name.to_string());
        let Some(pack_int_id) = midx
            .pack_names
            .iter()
            .position(|candidate| candidate == &idx_name)
        else {
            return Ok(None);
        };
        Ok(midx
            .objects
            .iter()
            .find(|entry| entry.pack_int_id == pack_int_id as u32 && entry.offset == offset)
            .map(|entry| entry.oid))
    }

    fn find_midx_pack_containing(
        &self,
        pack_dir: &Path,
        oid: &ObjectId,
    ) -> Result<Option<PackLookup>> {
        let midx_path = pack_dir.join("multi-pack-index");
        if let Some(midx) = self.cached_multi_pack_index_oid_lookup(&midx_path)?
            && let Some(pack_lookup) = self.midx_oid_lookup_pack_paths(pack_dir, &midx, oid)?
        {
            return Ok(Some(pack_lookup));
        }
        self.find_incremental_midx_pack_containing(pack_dir, oid)
    }

    fn midx_oid_lookup_pack_paths(
        &self,
        pack_dir: &Path,
        midx: &MultiPackIndexOidLookup,
        oid: &ObjectId,
    ) -> Result<Option<PackLookup>> {
        let Some(entry) = midx.find(oid)? else {
            return Ok(None);
        };
        let Some(pack_name) = midx.pack_name(entry.pack_int_id) else {
            return Err(GitError::InvalidFormat(
                "multi-pack-index object points past pack table".into(),
            ));
        };
        let pack_file_name = pack_name
            .strip_suffix(".idx")
            .map(|stem| format!("{stem}.pack"))
            .unwrap_or_else(|| pack_name.to_string());
        let pack = pack_dir.join(pack_file_name);
        Ok(Some(PackLookup::from_path(pack, entry.offset)))
    }

    fn find_incremental_midx_pack_containing(
        &self,
        pack_dir: &Path,
        oid: &ObjectId,
    ) -> Result<Option<PackLookup>> {
        let chain = read_incremental_midx_chain(pack_dir)?;
        if chain.is_empty() {
            return Ok(None);
        }
        let midx_dir = pack_dir.join("multi-pack-index.d");
        for checksum in chain.iter().rev() {
            let path = midx_dir.join(format!("multi-pack-index-{checksum}.midx"));
            if !path.exists() {
                continue;
            }
            let bytes = load_multi_pack_index_lookup_data(&path)?;
            let midx = match MultiPackIndexOidLookup::parse(bytes, self.format) {
                Ok(midx) => midx,
                Err(_) => continue,
            };
            if let Some(pack_lookup) = self.midx_oid_lookup_pack_paths(pack_dir, &midx, oid)? {
                return Ok(Some(pack_lookup));
            }
        }
        Ok(None)
    }

    fn cached_loaded_multi_pack_index_oid_lookup(&self) -> Option<Arc<MultiPackIndexOidLookup>> {
        let midx_path = self.objects_dir.join("pack").join("multi-pack-index");
        let cache = self.multi_pack_oid_lookups.lock().ok()?;
        cache.get(&midx_path).map(Arc::clone)
    }

    /// The pack registry for `pack_dir` *only if already scanned and cached* —
    /// never touches the filesystem. Used by the lookup hot path to skip
    /// per-object pack-dir metadata checks once a handle is warm. A cold cache
    /// returns `None`, so the caller falls back to the scanning path. A complete
    /// miss still forces one rescan, preserving the new-pack discovery semantics.
    fn cached_loaded_pack_registry(
        &self,
        _pack_dir: &Path,
    ) -> Result<Option<Arc<PackRegistrySnapshot>>> {
        let cache = match self.pack_registry.lock() {
            Ok(cache) => cache,
            Err(_) => return Ok(None),
        };
        Ok(cache.as_ref().map(Arc::clone))
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

fn pack_dir_modified(pack_dir: &Path) -> Result<Option<std::time::SystemTime>> {
    match fs::metadata(pack_dir) {
        Ok(metadata) => Ok(metadata.modified().ok()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

/// Scan `pack_dir` for `.idx` files that have a matching `.pack` sibling and
/// parse each index into a registered pack. An `.idx` without its `.pack` is
/// skipped (an orphan index cannot serve objects), matching the prior per-read
/// behavior.
fn scan_pack_registry(pack_dir: &Path, _format: ObjectFormat) -> Result<PackRegistrySnapshot> {
    let modified = pack_dir_modified(pack_dir)?;
    let entries = match fs::read_dir(pack_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PackRegistrySnapshot::new(
                PackDirFingerprint {
                    modified,
                    idx_count: 0,
                    pack_count: 0,
                },
                Vec::new(),
            ));
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };

    let mut idx_paths = Vec::new();
    let mut idx_count = 0;
    let mut pack_count = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("idx") => {
                idx_count += 1;
                idx_paths.push(path);
            }
            Some("pack") => {
                pack_count += 1;
            }
            _ => {}
        }
    }

    let mut packs = Vec::new();
    for idx in idx_paths {
        let pack = idx.with_extension("pack");
        let Ok(metadata) = fs::metadata(&pack) else {
            continue;
        };
        let modified = pack_sort_modified(&metadata);
        packs.push((
            modified,
            metadata.len(),
            Arc::new(RegisteredPack::new(idx, pack)),
        ));
    }
    // Git keeps a most-recently-used pack order; seed ours with newer/larger
    // packs before falling back to the path. In repositories with many packs,
    // this avoids parsing a long run of unrelated `.idx` files before the first
    // lookup establishes the recent-pack hint.
    packs.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.idx.cmp(&right.2.idx))
    });
    let packs = packs.into_iter().map(|(_, _, pack)| pack).collect();
    Ok(PackRegistrySnapshot::new(
        PackDirFingerprint {
            modified,
            idx_count,
            pack_count,
        },
        packs,
    ))
}

fn pack_sort_modified(metadata: &fs::Metadata) -> (u64, u32) {
    metadata
        .modified()
        .ok()
        .and_then(|modified| {
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        })
        .unwrap_or((0, 0))
}

/// Whether two pack registries reference the same pack/index paths (order is
/// already normalized by [`scan_pack_registry`]).
fn same_registered_pack_set(left: &[Arc<RegisteredPack>], right: &[Arc<RegisteredPack>]) -> bool {
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
    fn is_promised_object(&self, oid: &ObjectId) -> bool {
        // Gate on a configured promisor remote, exactly like git's
        // `is_promisor_object` (which short-circuits when
        // `repo_has_promisor_remote()` is false). Without this, a `.promisor`
        // sidecar left in an ordinary repository would wrongly excuse missing
        // objects from fsck connectivity checks.
        self.promisor_remote_present && self.promisor_objects().contains(oid)
    }

    fn has_shallow_grafts(&self) -> bool {
        !self
            .shallow_grafts
            .get_or_init(|| {
                let shallow_file = self
                    .objects_dir
                    .parent()
                    .map(|git_dir| git_dir.join("shallow"));
                match shallow_file {
                    Some(path) => read_shallow_grafts(&path, self.format),
                    None => HashSet::new(),
                }
            })
            .is_empty()
    }

    fn is_shallow_graft(&self, oid: &ObjectId) -> bool {
        self.shallow_grafts
            .get_or_init(|| {
                let shallow_file = self
                    .objects_dir
                    .parent()
                    .map(|git_dir| git_dir.join("shallow"));
                match shallow_file {
                    Some(path) => read_shallow_grafts(&path, self.format),
                    None => HashSet::new(),
                }
            })
            .contains(oid)
    }

    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        if let Some(object) = implied_empty_tree_object(self.format, oid) {
            return Ok(object);
        }
        // A corrupt loose copy must not shadow a good packed copy: git's
        // `oid_object_info_extended` consults every source, so a repacked object
        // whose loose file was later corrupted still reads fine from the pack. If
        // a packed copy exists, prefer it WITHOUT touching the corrupt loose file
        // (which would otherwise emit a spurious `inflate:` diagnostic on each
        // probe). Only when no pack copy exists do we read (and, if corrupt,
        // surface the error from) the loose file.
        if let Some(pack_lookup) = self.find_pack_containing(oid)? {
            match self.read_packed_object_at_lookup(oid, &pack_lookup) {
                Ok(object) => return Ok(object),
                Err(GitError::NotFound(_)) => {}
                // A corrupt packed copy must not be fatal when another good copy
                // exists: git's `oid_object_info_extended` keeps consulting the
                // remaining sources (loose, other packs, alternates) when a pack
                // read fails. Fall through to the loose/other-pack probes and
                // only surface the packed error if every source comes up empty.
                Err(packed_err) => {
                    if let Ok(object) = self.loose.read_object(oid) {
                        return Ok(object);
                    }
                    // Try any *other* pack that also holds the object (a
                    // redundant copy survives one pack's corruption).
                    if let Some(object) =
                        self.read_packed_object_from_other_packs(oid, &pack_lookup)?
                    {
                        return Ok(object);
                    }
                    for alternate in &self.alternates {
                        if let Ok(object) =
                            Self::without_alternates(alternate, self.format).read_object(oid)
                        {
                            return Ok(object);
                        }
                    }
                    return Err(packed_err);
                }
            }
        }
        let loose_err = match self.loose.read_object(oid) {
            Ok(object) => return Ok(object),
            Err(GitError::NotFound(_)) => None,
            Err(err) => Some(err),
        };
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
        // Hard miss against every store. If an earlier enumeration built a loose
        // cache, an object written loose afterward by a sibling handle could have
        // been skipped above. Mirror git's `oid_object_info_extended`
        // reprepare-on-miss: drop stale cache state and retry an exact loose path
        // probe once before declaring the object missing.
        self.loose.invalidate_cache();
        match self.loose.read_object(oid) {
            Ok(object) => return Ok(object),
            Err(GitError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
        // No good copy in any store. If the local loose copy was corrupt (not
        // merely absent), surface that error — it is more specific than a plain
        // "not found".
        if let Some(err) = loose_err {
            return Err(err);
        }
        Err(GitError::object_not_found_in(
            *oid,
            MissingObjectContext::Read,
        ))
    }
}

impl FileObjectDatabase {
    fn promisor_objects(&self) -> &HashSet<ObjectId> {
        self.promisor_objects.get_or_init(|| {
            let mut promised =
                promisor_pack_object_ids(&self.objects_dir, self.format).unwrap_or_default();
            let mut pending = promised.iter().copied().collect::<Vec<_>>();
            while let Some(oid) = pending.pop() {
                let Ok(object) = self.read_object(&oid) else {
                    continue;
                };
                for link in promisor_object_links(self.format, &object) {
                    if promised.insert(link) {
                        pending.push(link);
                    }
                }
            }
            promised
        })
    }

    fn freshen_existing_object(&self, oid: &ObjectId) -> Result<bool> {
        if self.freshen_loose_object(oid)? {
            return Ok(true);
        }
        if self.freshen_packed_object(oid)? {
            return Ok(true);
        }
        for alternate in &self.alternates {
            if Self::without_alternates(alternate, self.format).freshen_existing_object(oid)? {
                return Ok(true);
            }
        }
        // A previous negative loose-cache probe may predate a sibling write.
        self.loose.invalidate_cache();
        self.freshen_loose_object(oid)
    }

    fn freshen_loose_object(&self, oid: &ObjectId) -> Result<bool> {
        let path = self.loose.object_path(oid)?;
        freshen_file_mtime(&path)
    }

    fn freshen_packed_object(&self, oid: &ObjectId) -> Result<bool> {
        let Some(pack_lookup) = self.find_pack_containing(oid)? else {
            return Ok(false);
        };
        freshen_file_mtime(pack_lookup.pack_path())
    }
}

fn freshen_file_mtime(path: &Path) -> Result<bool> {
    let file = match fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    file.set_modified(std::time::SystemTime::now())
        .map_err(|err| GitError::Io(err.to_string()))?;
    Ok(true)
}

fn promisor_pack_object_ids(objects_dir: &Path, format: ObjectFormat) -> Result<HashSet<ObjectId>> {
    let pack_dir = objects_dir.join("pack");
    let mut oids = HashSet::new();
    if !pack_dir.exists() {
        return Ok(oids);
    }
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        if !path.with_extension("pack").exists() || !path.with_extension("promisor").exists() {
            continue;
        }
        let index = PackIndex::parse(&fs::read(path)?, format)?;
        oids.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(oids)
}

fn promisor_object_links(format: ObjectFormat, object: &EncodedObject) -> Vec<ObjectId> {
    match object.object_type {
        ObjectType::Commit => Commit::parse_ref(format, &object.body)
            .map(|commit| {
                let mut links = Vec::with_capacity(commit.parents.len() + 1);
                links.push(commit.tree);
                links.extend(commit.parents);
                links
            })
            .unwrap_or_default(),
        ObjectType::Tree => TreeEntries::new(format, &object.body)
            .filter_map(|entry| entry.ok().map(|entry| entry.oid))
            .collect(),
        ObjectType::Tag => Tag::parse_ref(format, &object.body)
            .map(|tag| vec![tag.object])
            .unwrap_or_default(),
        ObjectType::Blob => Vec::new(),
    }
}

impl ObjectWriter for FileObjectDatabase {
    fn write_object(&self, object: EncodedObject) -> Result<ObjectId> {
        // Mirror git's freshen semantics (`write_object_file`:
        // `freshen_packed_object || freshen_loose_object`): an object already
        // present anywhere in the database is not written again, but its backing
        // loose object or pack is touched so concurrent GC treats it as recent.
        let oid = object.object_id(self.format)?;
        if self.freshen_existing_object(&oid)? {
            return Ok(oid);
        }
        self.loose.write_object(object)
    }
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

/// git's `error:`-level diagnostic when the loose framing header cannot be inflated at
/// all (object-file.c `loose_object_info`, the `ULHR_BAD` arm: `error(_("unable to
/// unpack %s header"), ...)`).
fn loose_unpack_header_failed(oid: &ObjectId) -> GitError {
    GitError::InvalidObject(format!("unable to unpack {oid} header"))
}

/// git-zlib.c's `error("inflate: %s (%s)", ...)` text for an inflate failure whose
/// cause is identifiable from the zlib stream header. The checks mirror zlib's own
/// `inflate()` HEAD-state validation, in order: the FCHECK checksum over CMF+FLG,
/// the compression method, the window size, and the FDICT preset-dictionary bit
/// (zlib reports `Z_NEED_DICT` with a NULL `msg`, which git renders as
/// "(no message)"). Failures past the stream header return `None`: flate2 does not
/// surface zlib's per-case `msg` strings, so no diagnostic is fabricated for them.
fn inflate_header_diagnostic(input: &[u8]) -> Option<&'static str> {
    let [cmf, flg, ..] = *input else { return None };
    if ((u16::from(cmf) << 8) | u16::from(flg)) % 31 != 0 {
        return Some("inflate: data stream error (incorrect header check)");
    }
    if cmf & 0x0f != 8 {
        return Some("inflate: data stream error (unknown compression method)");
    }
    if cmf >> 4 > 7 {
        return Some("inflate: data stream error (invalid window size)");
    }
    if flg & 0x20 != 0 {
        return Some("inflate: needs dictionary (no message)");
    }
    None
}

/// Print the `error: inflate: ...` line git's zlib wrapper emits the moment
/// `inflate()` fails, when the failure is classifiable from the stream header.
fn emit_inflate_diagnostic(input: &[u8]) {
    if let Some(diagnostic) = inflate_header_diagnostic(input) {
        eprintln!("error: {diagnostic}");
    }
}

/// Integrity verdict for a single loose object file, as classified by
/// [`LooseObjectStore::verify_object`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LooseObjectIntegrity {
    /// Inflated, parsed, and re-hashed to its path-derived oid.
    Ok,
    /// Readable and well-formed, but its content hashes to a different oid
    /// (a loose file stored under the wrong path).
    HashMismatch { actual: ObjectId },
    /// Unreadable: corrupt zlib stream, truncated content, or unparseable header.
    /// The `error:`-level diagnostics were already printed to stderr.
    Corrupt,
}

#[derive(Debug, Clone)]
pub struct LooseObjectStore {
    objects_dir: PathBuf,
    format: ObjectFormat,
    /// Lazily-populated set of loose object ids present on disk, mirroring git's
    /// `loose_objects_cache` (object-file.c). A lookup scans the queried
    /// `objects/XX/` fanout once; afterward misses in that fanout are in-memory
    /// checks instead of failed exact-path opens. Shared across
    /// `FileObjectDatabase` clones via `Arc` so a write through one handle is
    /// visible to reads through another; cleared by `refresh_read_cache` so
    /// objects installed out-of-band (fetch, repack) become visible. Writes
    /// extend the set in place rather than invalidating it.
    loose_cache: Arc<Mutex<LoosePresenceCache>>,
}

impl LooseObjectStore {
    pub fn new(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        Self {
            objects_dir: objects_dir.into(),
            format,
            loose_cache: Arc::new(Mutex::new(LoosePresenceCache::default())),
        }
    }

    /// Whether `oid` is present according to the loose-object cache, populating
    /// the cache on first use. Returns `None` when the lock cannot be trusted or
    /// the scan fails; callers should fall back to an exact filesystem probe in
    /// that case so a cache-building problem cannot change read semantics.
    fn cached_loose_presence(&self, oid: &ObjectId) -> Option<bool> {
        let mut guard = self.loose_cache.lock().ok()?;
        let fanout = oid.as_bytes()[0];
        if !guard.loaded_fanouts.contains(&fanout) {
            // Learn (once) which `objects/XX/` dirs exist via a single
            // `read_dir(objects/)`. If this id's fanout dir is absent, no loose
            // object can live there — skip the per-fanout `read_dir` that would
            // otherwise miss with ENOENT. For an all-packed repo (every fanout
            // absent) this collapses the whole loose-probe cost to one
            // `read_dir(objects/)`.
            if guard.present_fanouts.is_none() {
                guard.present_fanouts = Some(present_loose_fanouts(&self.objects_dir).ok()?);
            }
            let fanout_present = guard
                .present_fanouts
                .as_ref()
                .is_some_and(|present| present.contains(&fanout));
            if fanout_present {
                collect_loose_fanout_object_ids(
                    &self.objects_dir,
                    self.format,
                    fanout,
                    &mut guard.objects,
                )
                .ok()?;
            }
            // Mark the fanout loaded regardless: an absent fanout contributes no
            // ids, and the `present_fanouts` set already proved it empty, so we
            // never need to rescan it (a later loose write into a previously
            // absent fanout goes through `note_loose_write`, which records the
            // id directly, or `invalidate_cache`, which clears `present_fanouts`
            // so the next probe re-learns the dir set).
            guard.loaded_fanouts.insert(fanout);
        }
        Some(guard.objects.contains(oid))
    }

    /// Populate the loose-object cache and return the sorted ids. This mirrors
    /// git's `odb_loose_cache` lazy fill and is reserved for operations that
    /// really need loose-object enumeration.
    fn loose_object_ids_cached(&self) -> Result<Vec<ObjectId>> {
        if let Ok(mut guard) = self.loose_cache.lock() {
            guard.objects = loose_object_id_set(&self.objects_dir, self.format)?;
            guard.loaded_fanouts = (0..=u8::MAX).collect();
            let mut ids = guard.objects.iter().copied().collect::<Vec<_>>();
            ids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            return Ok(ids);
        }
        loose_object_ids(&self.objects_dir, self.format)
    }

    /// Record `oid` as present in loose storage so subsequent reads find it
    /// without a rescan. A no-op when the cache has not been populated yet (the
    /// eventual lazy scan will pick the object up) or the lock is poisoned.
    fn note_loose_write(&self, oid: ObjectId) {
        if let Ok(mut guard) = self.loose_cache.lock() {
            // Keep the present-fanout set coherent: writing this object created
            // (or kept) its `objects/XX/` dir, so a sibling id in the same fanout
            // must be scannable on its next probe rather than short-circuited as
            // an absent fanout.
            let fanout = oid.as_bytes()[0];
            if let Some(present) = guard.present_fanouts.as_mut() {
                present.insert(fanout);
            }
            guard.objects.insert(oid);
        }
    }

    /// Drop the in-memory loose set so the next access rescans the fanout. Called
    /// by `FileObjectDatabase::refresh_read_cache` after out-of-band installs.
    pub(crate) fn invalidate_cache(&self) {
        if let Ok(mut guard) = self.loose_cache.lock() {
            *guard = LoosePresenceCache::default();
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        Self::new(repository_objects_dir(git_dir), format)
    }

    fn validate_oid_format(&self, oid: &ObjectId) -> Result<()> {
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        Ok(())
    }

    pub fn object_path(&self, oid: &ObjectId) -> Result<PathBuf> {
        self.validate_oid_format(oid)?;
        let hex = oid.to_hex();
        Ok(self.objects_dir.join(&hex[..2]).join(&hex[2..]))
    }

    pub fn exists(&self, oid: &ObjectId) -> Result<bool> {
        self.validate_oid_format(oid)?;
        if self.cached_loose_presence(oid) == Some(false) {
            return Ok(false);
        }
        let path = self.object_path(oid)?;
        Ok(path.exists())
    }

    pub fn disk_size(&self, oid: &ObjectId) -> Result<Option<u64>> {
        self.validate_oid_format(oid)?;
        if self.cached_loose_presence(oid) == Some(false) {
            return Ok(None);
        }
        let path = self.object_path(oid)?;
        match fs::metadata(path) {
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
        self.validate_oid_format(oid)?;
        if self.cached_loose_presence(oid) == Some(false) {
            return Ok(None);
        }
        let path = self.object_path(oid)?;
        let compressed = match fs::read(&path) {
            Ok(compressed) => compressed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        match inflate_loose_header(&compressed)? {
            LooseHeader::Ok(header) => {
                let header = std::str::from_utf8(&header)
                    .map_err(|err| GitError::InvalidObject(err.to_string()))?;
                let (kind, size) = header
                    .split_once(' ')
                    .ok_or_else(|| GitError::InvalidObject("missing object size".into()))?;
                let object_type = kind.parse::<ObjectType>()?;
                let size = size
                    .parse::<u64>()
                    .map_err(|_| GitError::InvalidObject("invalid object size".into()))?;
                Ok(Some((object_type, size)))
            }
            LooseHeader::Bad => {
                // git's ULHR_BAD: the zlib wrapper's `error: inflate: ...` line, then
                // "unable to unpack <oid> header".
                emit_inflate_diagnostic(compressed.get(..2).unwrap_or(&compressed));
                Err(loose_unpack_header_failed(oid))
            }
            LooseHeader::TooLong => {
                // git inflates only the first `MAX_LOOSE_HEADER_LEN` bytes
                // (object-file.c `unpack_loose_header`) and reports ULHR_TOO_LONG when
                // no NUL terminator lands within them — whether the stream simply ends
                // early or overflows the window. Both collapse to the same diagnostic.
                Err(loose_header_too_long(oid))
            }
        }
    }

    /// Loose object ids in this store, sorted by hex.
    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        self.loose_object_ids_cached()
    }

    /// fsck's loose-object integrity probe, mirroring C git's `read_loose_object`
    /// (object-file.c) as called from `fsck_loose` (builtin/fsck.c): inflate and
    /// parse the file at `oid`'s loose path, then re-hash its content against the
    /// path-derived oid. `display_path` appears verbatim in the `error:`-level
    /// diagnostics — the path-form messages of `read_loose_object` ("unable to
    /// unpack header of <path>"), unlike the oid-form messages of the normal read
    /// path. Returns `Ok(None)` when no loose file exists for `oid`.
    pub fn verify_object(
        &self,
        oid: &ObjectId,
        display_path: &str,
    ) -> Result<Option<LooseObjectIntegrity>> {
        let path = self.object_path(oid)?;
        let compressed = match fs::read(&path) {
            Ok(compressed) => compressed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut framed = Vec::new();
        if decoder.read_to_end(&mut framed).is_err() {
            emit_inflate_diagnostic(&compressed);
            // git inflates the header first (`unpack_loose_header`), then the body
            // (`unpack_loose_rest`). If the header inflated (its NUL is visible in
            // the partial output) but the body broke, that is a *content*
            // corruption: git's `unpack_loose_rest` prints `corrupt loose object
            // '<oid>'` (status != Z_STREAM_END), then `read_loose_object` adds
            // `unable to unpack contents of <path>`. If inflation died before the
            // header materialized, only the header message fires.
            if framed_loose_header_terminated(&framed) {
                eprintln!("error: corrupt loose object '{oid}'");
                eprintln!("error: unable to unpack contents of {display_path}");
            } else {
                eprintln!("error: unable to unpack header of {display_path}");
            }
            return Ok(Some(LooseObjectIntegrity::Corrupt));
        }
        if !framed_loose_header_terminated(&framed) {
            // ULHR_TOO_LONG collapses into the same path-form message here: C's
            // `read_loose_object` treats every non-OK `unpack_loose_header` alike.
            eprintln!("error: unable to unpack header of {display_path}");
            return Ok(Some(LooseObjectIntegrity::Corrupt));
        }
        // git's `unpack_loose_rest`/`check_stream_oid` reject trailing bytes after
        // the zlib stream: a fully-inflated object whose compressed input was not
        // entirely consumed is `garbage at end of loose object '<oid>'`, then
        // `object corrupt or missing: <path>` from `fsck_loose`. (read_to_end
        // stops at Z_STREAM_END and silently ignores the trailing bytes, so we
        // compare consumed input against the file size ourselves.)
        if (decoder.total_in() as usize) < compressed.len() {
            // git's `unpack_loose_rest` prints `garbage at end of loose object`
            // then returns NULL, so `read_loose_object` also prints `unable to
            // unpack contents of <path>`.
            eprintln!("error: garbage at end of loose object '{oid}'");
            eprintln!("error: unable to unpack contents of {display_path}");
            return Ok(Some(LooseObjectIntegrity::Corrupt));
        }
        // A truncated object can inflate to a clean stream end yet yield fewer
        // body bytes than the header's declared size. git's `unpack_loose_rest`
        // inflates exactly `size` bytes and, finding the stream ends short,
        // prints `corrupt loose object '<oid>'`; `read_loose_object` then adds
        // `unable to unpack contents of <path>`. Detect the short body here so it
        // is not misreported as a header-parse failure.
        if let Some(declared) = loose_header_declared_size(&framed) {
            let nul = framed.iter().position(|&b| b == 0).unwrap_or(framed.len());
            let body_len = framed.len() - (nul + 1).min(framed.len());
            if body_len < declared {
                eprintln!("error: corrupt loose object '{oid}'");
                eprintln!("error: unable to unpack contents of {display_path}");
                return Ok(Some(LooseObjectIntegrity::Corrupt));
            }
        }
        let Ok(object) = parse_framed_object(&framed) else {
            // Distinguish git's two header-parse failures: a structurally valid
            // `"<word> <size>\0"` header whose *type word* is not a known object
            // type yields `unable to parse type from header '<header>'`, while a
            // genuinely malformed header yields `unable to parse header`.
            if let Some(header) = loose_header_with_unknown_type(&framed) {
                eprintln!("error: unable to parse type from header '{header}' of {display_path}");
            } else {
                eprintln!("error: unable to parse header of {display_path}");
            }
            return Ok(Some(LooseObjectIntegrity::Corrupt));
        };
        let actual = object.object_id(self.format)?;
        if &actual != oid {
            return Ok(Some(LooseObjectIntegrity::HashMismatch { actual }));
        }
        Ok(Some(LooseObjectIntegrity::Ok))
    }
}

/// Whether the inflated framing bytes contain the header's NUL terminator within
/// git's `MAX_HEADER_LEN` window (object-file.c `unpack_loose_header`'s success
/// condition).
fn framed_loose_header_terminated(framed: &[u8]) -> bool {
    framed
        .iter()
        .take(MAX_LOOSE_HEADER_LEN)
        .any(|byte| *byte == 0)
}

/// If the framing has a structurally valid `"<word> <size>\0"` header whose body
/// length matches `<size>` but whose `<word>` is not a known object type, return
/// the header string (the bytes before the NUL). Mirrors git's
/// `parse_loose_header` reporting `unable to parse type from header '<header>'`.
fn loose_header_with_unknown_type(framed: &[u8]) -> Option<String> {
    let nul = framed.iter().position(|&b| b == 0)?;
    let header = std::str::from_utf8(&framed[..nul]).ok()?;
    let (kind, size) = header.split_once(' ')?;
    let size: usize = size.parse().ok()?;
    // Body length must match the declared size (otherwise it is a different
    // corruption, handled by the generic path).
    if framed.len() - (nul + 1) != size {
        return None;
    }
    // A known type word would have parsed successfully upstream; only return
    // when the word is genuinely unknown.
    if kind.parse::<ObjectType>().is_ok() {
        return None;
    }
    Some(header.to_string())
}

/// The size declared in a loose object's `"<type> <size>\0"` header, if the
/// header is structurally a `<word> <decimal-size>` pair. Used to detect a body
/// inflated short of its declared length (a truncated object).
fn loose_header_declared_size(framed: &[u8]) -> Option<usize> {
    let nul = framed.iter().position(|&b| b == 0)?;
    let header = std::str::from_utf8(&framed[..nul]).ok()?;
    let (_kind, size) = header.split_once(' ')?;
    size.parse::<usize>().ok()
}

/// Read up to `prefix.len()` bytes from the start of `file`, returning how many
/// were available (short only when the file itself is shorter).
/// Outcome of inflating a loose object's header, mirroring git's
/// `unpack_loose_header` result codes (object-file.c `enum
/// unpack_loose_header_result`).
enum LooseHeader {
    /// ULHR_OK: a NUL-terminated header was found within the window. Carries the
    /// header bytes up to (not including) the NUL.
    Ok(Vec<u8>),
    /// ULHR_BAD: the zlib stream would not inflate (status != Z_OK/Z_STREAM_END).
    Bad,
    /// ULHR_TOO_LONG: the inflated output filled the header window with no NUL.
    TooLong,
}

/// Inflate a loose object's *header* exactly as git's `unpack_loose_header` does
/// (object-file.c): a single bounded inflate into a `MAX_LOOSE_HEADER_LEN`-byte
/// output buffer, then look for the header-terminating NUL in what came out.
///
/// The byte budget is load-bearing for corruption parity: git inflates only up to
/// `MAX_HEADER_LEN` (32) bytes of *output* before stopping, so a `cat-file -s`/`-t`
/// header read detects a zlib data error only when it lands within those first 32
/// inflated bytes (the header plus the start of the body for a small object) — and
/// silently returns the header for corruption buried deeper in the body, which the
/// full-object read path catches instead. A byte-by-byte loop that stopped at the
/// NUL would never inflate into the corrupt region and miss the bit-error case
/// (t1060 "getting type of a corrupt blob fails"); feeding too much output budget
/// would over-detect relative to git. So this matches git's exact window.
fn inflate_loose_header(compressed: &[u8]) -> Result<LooseHeader> {
    let mut out = [0u8; MAX_LOOSE_HEADER_LEN];
    let mut decompress = Decompress::new(true);
    // git feeds the whole mapped file as `avail_in` and inflates once into a
    // 32-byte `avail_out`; zlib stops at the output limit (Z_OK with avail_out==0)
    // or at the stream's end, propagating Z_DATA_ERROR for a corrupt stream.
    let status = decompress.decompress(compressed, &mut out, FlushDecompress::None);
    let produced = decompress.total_out() as usize;
    match status {
        Ok(_) => {
            let window = &out[..produced.min(MAX_LOOSE_HEADER_LEN)];
            match window.iter().position(|&byte| byte == 0) {
                Some(nul) => Ok(LooseHeader::Ok(window[..nul].to_vec())),
                // No NUL within the window: either the stream ended early or the
                // header overflows `MAX_LOOSE_HEADER_LEN`. git collapses both into
                // ULHR_TOO_LONG (object-file.c `unpack_loose_header`).
                None => Ok(LooseHeader::TooLong),
            }
        }
        // Any zlib error before a NUL materializes is git's ULHR_BAD.
        Err(_) => Ok(LooseHeader::Bad),
    }
}

impl ObjectReader for LooseObjectStore {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        self.validate_oid_format(oid)?;
        // Skip the `open()` (and its ENOENT) when an already-built loose cache
        // knows the id is absent. Without a cache, use an exact path probe; a
        // full fanout scan is far more expensive for one-shot packed-object reads.
        if self.cached_loose_presence(oid) == Some(false) {
            return Err(GitError::object_not_found_in(
                *oid,
                MissingObjectContext::Read,
            ));
        }
        let path = self.object_path(oid)?;
        let compressed = match fs::read(&path) {
            Ok(compressed) => compressed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(GitError::object_not_found_in(
                    *oid,
                    MissingObjectContext::Read,
                ));
            }
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut framed = Vec::new();
        if decoder.read_to_end(&mut framed).is_err() {
            emit_inflate_diagnostic(&compressed);
            // A stream that dies before the framing header materializes is git's
            // ULHR_BAD ("unable to unpack <oid> header"); with the header intact,
            // the body is what broke (`unpack_loose_rest`'s "corrupt loose
            // object").
            if !framed_loose_header_terminated(&framed) {
                return Err(loose_unpack_header_failed(oid));
            }
            return Err(GitError::InvalidObject(format!(
                "corrupt loose object '{oid}'"
            )));
        }
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
    fn write_object(&self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        let path = self.object_path(&oid)?;
        if path.exists() {
            self.note_loose_write(oid);
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
                // No fsync: git's default `core.fsync=none` fsyncs nothing on the
                // loose-object write path (object-file.c writes the temp file and
                // renames it without syncing unless `core.fsync` names
                // `loose-object`/`objects`/`all`, which it does not by default).
                // A per-object sync_all() here made `git add` of N files cost N
                // fsyncs — the dominant term in sley#27's 10x `add -u` slowdown —
                // for durability git itself does not provide by default. The
                // create_new temp + atomic rename below still guarantees the
                // object never appears half-written under its final name.
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
        self.note_loose_write(oid);
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
        let db = ObjectDatabase::new(ObjectFormat::Sha1);
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
        let store = LooseObjectStore::new(root.join("objects"), ObjectFormat::Sha1);
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
    fn read_header_detects_corruption_within_gits_header_window() {
        // git's `unpack_loose_header` inflates only the first MAX_HEADER_LEN (32)
        // bytes of output; a zlib data error inside that window makes `cat-file
        // -s`/`-t` fail (ULHR_BAD → "unable to unpack header"). A byte-by-byte
        // header read that stopped at the NUL would never inflate into the corrupt
        // region and would silently return a bogus size — the t1060 "getting type
        // of a corrupt blob fails" bug. Corrupt a byte inside the inflate stream of
        // a tiny object so the damage lands within the first 32 inflated bytes.
        let root = temp_root("sley-loose-header-corrupt");
        let store = LooseObjectStore::new(root.join("objects"), ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"content\n".to_vec());
        let oid = store
            .write_object(object)
            .expect("test operation should succeed");
        let path = store
            .object_path(&oid)
            .expect("test operation should succeed");
        let mut bytes = fs::read(&path).expect("test operation should succeed");
        // Offset 10 is inside the deflate stream (past the 2-byte zlib header) and,
        // for an 8-byte blob, decodes into the first 32 output bytes. Zero it to
        // break inflation, mirroring t1060's `corrupt_byte HEAD:content.t 10`.
        bytes[10] = 0;
        fs::write(&path, &bytes).expect("test operation should succeed");
        store.invalidate_cache();
        let err = store
            .read_header(&oid)
            .expect_err("corrupt loose header must fail like git's ULHR_BAD");
        let msg = err.to_string();
        assert!(
            msg.contains("unable to unpack") && msg.contains(&oid.to_hex()),
            "expected git's ULHR_BAD message, got: {msg}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn read_header_ignores_corruption_past_gits_header_window() {
        // Mirror git: corruption deeper than the 32-byte header window is NOT
        // detected by a header-only read (`cat-file -s` still returns the size);
        // the full-object read path catches it instead. Over-detecting here would
        // diverge from upstream on large objects with a clean header.
        let root = temp_root("sley-loose-header-deep-corrupt");
        let store = LooseObjectStore::new(root.join("objects"), ObjectFormat::Sha1);
        // Incompressible body so the deflate stream is long and a deep byte is well
        // past the 32 inflated header-window bytes.
        let body: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761)) as u8)
            .collect();
        let object = EncodedObject::new(ObjectType::Blob, body.clone());
        let oid = store
            .write_object(object)
            .expect("test operation should succeed");
        let path = store
            .object_path(&oid)
            .expect("test operation should succeed");
        let mut bytes = fs::read(&path).expect("test operation should succeed");
        let deep = bytes.len() / 2;
        bytes[deep] ^= 0xff;
        fs::write(&path, &bytes).expect("test operation should succeed");
        store.invalidate_cache();
        let header = store
            .read_header(&oid)
            .expect("header-only read must still succeed for deep body corruption");
        assert_eq!(header, Some((ObjectType::Blob, body.len() as u64)));
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
    fn file_database_loose_cache_observes_same_process_write_after_miss() {
        let root = temp_root("sley-file-odb-loose-cache-write");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let object = EncodedObject::new(ObjectType::Blob, b"written after miss\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        assert!(matches!(db.read_object(&oid), Err(GitError::NotFound(_))));
        db.loose()
            .write_object(object.clone())
            .expect("test operation should succeed");

        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn present_loose_fanouts_lists_only_existing_two_hex_dirs() {
        let root = temp_root("sley-present-fanouts");
        let objects = root.join("objects");
        fs::create_dir_all(objects.join("ab")).expect("test operation should succeed");
        fs::create_dir_all(objects.join("0f")).expect("test operation should succeed");
        // Non-fanout siblings git keeps under objects/ must be ignored.
        fs::create_dir_all(objects.join("pack")).expect("test operation should succeed");
        fs::create_dir_all(objects.join("info")).expect("test operation should succeed");
        // A 2-char-but-non-hex dir, and a regular file with a 2-hex name, are not
        // fanouts.
        fs::create_dir_all(objects.join("zz")).expect("test operation should succeed");
        fs::write(objects.join("ff"), b"not a dir").expect("test operation should succeed");

        let present = present_loose_fanouts(&objects).expect("test operation should succeed");
        assert_eq!(
            present,
            HashSet::from([0xab, 0x0f]),
            "only the genuine two-hex fanout directories should be reported"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn present_loose_fanouts_empty_when_objects_dir_absent() {
        let root = temp_root("sley-present-fanouts-absent");
        fs::create_dir_all(&root).expect("test operation should succeed");
        // No objects dir at all (e.g. an all-packed bare layout before any loose
        // write): the helper reports an empty set rather than erroring.
        let present =
            present_loose_fanouts(&root.join("objects")).expect("test operation should succeed");
        assert!(present.is_empty());
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn packed_only_repo_read_does_not_probe_loose_fanout_dirs() {
        // Regression for the loose-first statx floor: an all-packed repo must read
        // objects without ever opendir()-ing a per-id `objects/XX/` fanout, because
        // none exist. The present-fanout set is learned from one `read_dir(objects/)`.
        let root = temp_root("sley-packed-no-loose-probe");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"packed only\n".to_vec());
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
        // Header read takes the loose-first path; it must still resolve from the pack
        // and learn that the object's fanout dir is absent.
        assert_eq!(
            db.read_object_header(&oid)
                .expect("test operation should succeed"),
            Some((ObjectType::Blob, object.body.len() as u64))
        );
        assert_eq!(read_object_for_assert(&db, &oid), object);

        // No fanout dir was created by the read (we never wrote loose), and the
        // cached present-fanout set is the empty set — so further probes short-circuit.
        let fanout_hex = format!("{:02x}", oid.as_bytes()[0]);
        assert!(
            !git_dir.join("objects").join(&fanout_hex).exists(),
            "reading a packed object must not create its loose fanout dir"
        );
        if let Ok(guard) = db.loose().loose_cache.lock() {
            assert_eq!(
                guard.present_fanouts.as_ref(),
                Some(&HashSet::new()),
                "an all-packed repo must learn zero present fanouts"
            );
        }
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn loose_object_in_existing_fanout_still_resolves_through_cache() {
        // The optimization must not hide a real loose object: when its fanout dir
        // exists, the per-fanout scan still runs and the read succeeds.
        let root = temp_root("sley-loose-resolves");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"a genuine loose object\n".to_vec());
        let oid = db
            .write_object(object.clone())
            .expect("test operation should succeed");
        // Drop all in-memory state so the read must re-learn fanouts from disk.
        db.refresh_read_cache();
        assert_eq!(
            db.read_object_header(&oid)
                .expect("test operation should succeed"),
            Some((ObjectType::Blob, object.body.len() as u64))
        );
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn loose_write_into_previously_absent_fanout_is_found_after_cache_built() {
        // Cache-coherence gate: a packed-only read first learns "all fanouts
        // absent". A subsequent loose write must NOT be permanently masked by that
        // negative present-fanout set — the just-written object reads back.
        let root = temp_root("sley-new-loose-after-cache");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("test operation should succeed");
        // Seed one packed object and read it, which warms the present-fanout set to
        // empty (no loose dirs exist yet).
        let packed = EncodedObject::new(ObjectType::Blob, b"seed packed\n".to_vec());
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&packed))
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
        let packed_oid = packed
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        // Read the packed object so the loose cache learns "zero present fanouts".
        assert_eq!(read_object_for_assert(&db, &packed_oid), packed);

        // Now write a NEW loose object through the same handle. Its fanout dir did
        // not exist when the cache learned the present set, but `note_loose_write`
        // must keep the read path coherent.
        let loose = EncodedObject::new(ObjectType::Blob, b"new loose into empty fanout\n".to_vec());
        let loose_oid = db
            .write_object(loose.clone())
            .expect("test operation should succeed");
        assert_eq!(
            db.read_object_header(&loose_oid)
                .expect("test operation should succeed"),
            Some((ObjectType::Blob, loose.body.len() as u64)),
            "a loose object written after the present-fanout cache was built must be found"
        );
        assert_eq!(read_object_for_assert(&db, &loose_oid), loose);
        // And the original packed object still resolves.
        assert_eq!(read_object_for_assert(&db, &packed_oid), packed);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn loose_copy_is_consulted_when_packed_copy_is_corrupt() {
        // Loose-shadows-packed precedence: git's `oid_object_info_extended` keeps a
        // good loose copy authoritative when the packed copy is unreadable. The
        // present-fanout optimization must not change this — the loose fanout dir
        // exists (we wrote it), so it is scanned and consulted.
        let root = temp_root("sley-loose-shadows-corrupt-pack");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"shadow me\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        // Write the object both packed and loose (content-addressed: same oid).
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        let pack_name = written.checksum.to_hex();
        let pack_path = pack_dir.join(format!("pack-{pack_name}.pack"));
        fs::write(&pack_path, written.pack).expect("test operation should succeed");
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        db.loose()
            .write_object(object.clone())
            .expect("test operation should succeed");

        // Corrupt the pack body so the packed read fails; the good loose copy must
        // still satisfy the read.
        let mut pack_bytes = fs::read(&pack_path).expect("test operation should succeed");
        let mid = pack_bytes.len() / 2;
        pack_bytes[mid] ^= 0xff;
        fs::write(&pack_path, &pack_bytes).expect("test operation should succeed");
        db.refresh_read_cache();

        assert_eq!(
            read_object_for_assert(&db, &oid),
            object,
            "a good loose copy must shadow a corrupt packed copy"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn object_presence_checker_observes_same_process_loose_write_after_miss() {
        let root = temp_root("sley-presence-checker-loose-cache-write");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let mut checker = db.presence_checker();

        let object = EncodedObject::new(ObjectType::Blob, b"checker loose after miss\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        assert!(
            !checker
                .contains(&oid)
                .expect("test operation should succeed")
        );
        db.loose()
            .write_object(object)
            .expect("test operation should succeed");

        assert!(
            checker
                .contains(&oid)
                .expect("test operation should succeed")
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn read_object_header_matches_full_read_for_loose_and_packed_and_delta() {
        let root = temp_root("sley-read-object-header");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

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
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

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
    fn object_storage_info_uses_midx_when_pack_sidecar_is_missing() {
        let root = temp_root("sley-object-storage-midx-fallback");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let first = EncodedObject::new(ObjectType::Blob, b"first packed object\n".to_vec());
        let second = EncodedObject::new(ObjectType::Blob, b"second packed object\n".to_vec());
        let written = PackFile::write_undeltified_sha1(&[first, second])
            .expect("test operation should succeed");
        let pack_name = written.checksum.to_hex();
        let pack_path = pack_dir.join(format!("pack-{pack_name}.pack"));
        let idx_path = pack_dir.join(format!("pack-{pack_name}.idx"));
        fs::write(&pack_path, &written.pack).expect("test operation should succeed");
        fs::write(&idx_path, &written.index).expect("test operation should succeed");

        let idx_name = idx_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test operation should succeed")
            .to_string();
        let midx_objects = written
            .entries
            .iter()
            .map(|entry| sley_pack::MultiPackIndexEntry {
                oid: entry.oid,
                pack_int_id: 0,
                offset: entry.offset,
                force_large_offset: false,
            })
            .collect::<Vec<_>>();
        let midx = MultiPackIndex::write(format, 1, &[idx_name], &midx_objects)
            .expect("test operation should succeed");
        fs::write(pack_dir.join("multi-pack-index"), midx).expect("test operation should succeed");

        let target = written
            .entries
            .iter()
            .min_by_key(|entry| entry.offset)
            .expect("test operation should succeed")
            .oid;
        let indexed_info = FileObjectDatabase::from_git_dir(&git_dir, format)
            .object_storage_info(&target)
            .expect("test operation should succeed")
            .expect("test operation should succeed");

        fs::remove_file(&idx_path).expect("test operation should succeed");
        let missing_idx_info = FileObjectDatabase::from_git_dir(&git_dir, format)
            .object_storage_info(&target)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(missing_idx_info, indexed_info);

        fs::write(&idx_path, &written.index).expect("test operation should succeed");
        fs::remove_file(&pack_path).expect("test operation should succeed");
        let missing_pack_info = FileObjectDatabase::from_git_dir(&git_dir, format)
            .object_storage_info(&target)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(missing_pack_info.disk_size, indexed_info.disk_size);
        assert_eq!(
            missing_pack_info.deltabase,
            zero_oid(format).expect("test operation should succeed")
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_resolves_unique_loose_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-loose");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
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
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
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
        let mut reader = pack.pack.as_slice();

        let result = db
            .install_raw_pack_from_reader(&mut reader)
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
    fn file_database_streams_raw_pack_install_to_packfile() {
        use std::io::Write as _;

        let root = temp_root("sley-file-odb-stream-raw-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"streamed raw pack\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let mut install = db
            .begin_raw_pack_install(pack.checksum, pack.pack.len() as u64)
            .expect("test operation should succeed");
        for chunk in pack.pack.chunks(5) {
            install
                .write_all(chunk)
                .expect("test operation should succeed");
        }
        let result = install.finish().expect("test operation should succeed");

        assert_eq!(result.pack_name, format!("pack-{}", pack.checksum.to_hex()));
        assert_eq!(result.object_ids, vec![oid]);
        assert_eq!(
            fs::read(&result.pack_path).expect("test operation should succeed"),
            pack.pack
        );
        assert!(result.index_path.exists());
        assert!(db.contains(&oid).expect("test operation should succeed"));
        assert_eq!(read_object_for_assert(&db, &oid), object);

        let bad_id = ObjectId::from_raw(ObjectFormat::Sha1, &[0x42; 20])
            .expect("test operation should succeed");
        let mut bad_install = db
            .begin_raw_pack_install(bad_id, pack.pack.len() as u64)
            .expect("test operation should succeed");
        bad_install
            .write_all(&pack.pack)
            .expect("test operation should succeed");
        assert!(
            bad_install.finish().is_err(),
            "checksum mismatch should reject the streamed pack"
        );
        assert!(
            !git_dir
                .join("objects")
                .join("pack")
                .join(format!("pack-{}.pack", bad_id.to_hex()))
                .exists()
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_installs_unknown_length_raw_pack_from_reader() {
        let root = temp_root("sley-file-odb-install-raw-pack-reader");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"reader streamed raw pack\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let mut reader = pack.pack.as_slice();

        let result = db
            .install_raw_pack_from_reader(&mut reader)
            .expect("test operation should succeed");

        assert_eq!(result.pack_name, format!("pack-{}", pack.checksum.to_hex()));
        assert_eq!(result.object_ids, vec![oid]);
        assert_eq!(
            fs::read(&result.pack_path).expect("test operation should succeed"),
            pack.pack
        );
        assert!(result.index_path.exists());
        assert!(db.contains(&oid).expect("test operation should succeed"));
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_rejects_unknown_length_raw_pack_with_trailing_bytes() {
        let root = temp_root("sley-file-odb-install-raw-pack-reader-trailing");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"trailing streamed raw pack\n".to_vec());
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let mut bytes = pack.pack;
        bytes.extend_from_slice(b"not part of the pack");
        let mut reader = bytes.as_slice();

        let err = db
            .install_raw_pack_from_reader(&mut reader)
            .expect_err("trailing bytes should be rejected");

        assert!(err.to_string().contains("trailing bytes after checksum"));
        let pack_dir = git_dir.join("objects").join("pack");
        let pack_entries = fs::read_dir(&pack_dir)
            .map(|entries| entries.count())
            .unwrap_or_default();
        assert_eq!(pack_entries, 0);
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
        let mut reader = pack.pack.as_slice();

        let result = db
            .install_raw_pack_from_reader_with_options(
                &mut reader,
                RawPackInstallOptions { promisor: true },
            )
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
        let source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
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
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

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
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

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

        let pack_path = root.join("reachable.pack");
        let pack_file = build_reachable_pack_file(
            &db,
            format,
            std::iter::once(oid),
            &HashSet::new(),
            &pack_path,
        )
        .expect("test operation should succeed")
        .expect("reachable pack file should be built");
        assert_eq!(pack_file.checksum, pack.checksum);
        assert_eq!(pack_file.pack_size, pack.pack.len() as u64);
        assert_eq!(pack_file.object_count, 1);
        assert_eq!(
            fs::read(&pack_file.pack_path).expect("test operation should succeed"),
            pack.pack
        );

        let mut streamed_pack = Vec::new();
        let streamed = write_reachable_pack_to_writer(
            &db,
            format,
            std::iter::once(oid),
            &HashSet::new(),
            &mut streamed_pack,
        )
        .expect("test operation should succeed")
        .expect("reachable pack should be streamed");
        assert_eq!(streamed.checksum, pack.checksum);
        assert_eq!(streamed.pack_size, pack.pack.len() as u64);
        assert_eq!(streamed.object_count, 1);
        assert_eq!(streamed_pack, pack.pack);

        let mut sink = std::io::sink();
        let dry_run = write_reachable_pack_to_writer(
            &db,
            format,
            std::iter::once(oid),
            &HashSet::new(),
            &mut sink,
        )
        .expect("test operation should succeed")
        .expect("reachable pack should stream to sink");
        assert_eq!(dry_run.checksum, pack.checksum);
        assert_eq!(dry_run.pack_size, pack.pack.len() as u64);
        assert_eq!(dry_run.object_count, 1);

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
    fn large_reachable_pack_streams_objects_by_id_windows() {
        let root = temp_root("sley-reachable-pack-streamed-large");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let mut roots = Vec::new();
        for idx in 0..(REACHABLE_PACK_STREAMING_MIN_OBJECTS + 5) {
            let object = EncodedObject::new(
                ObjectType::Blob,
                format!("streamed reachable blob {idx:04}\n").into_bytes(),
            );
            roots.push(
                db.write_object(object)
                    .expect("test operation should succeed"),
            );
        }

        let mut pack_bytes = Vec::new();
        let summary = write_reachable_pack_to_writer(
            &db,
            format,
            roots.iter().copied(),
            &HashSet::new(),
            &mut pack_bytes,
        )
        .expect("test operation should succeed")
        .expect("reachable pack should be streamed");
        assert_eq!(summary.object_count, roots.len());
        assert!(
            summary.delta_count > 0,
            "streamed large packs should still find deltas"
        );
        assert_eq!(summary.pack_size, pack_bytes.len() as u64);

        let parsed = PackFile::parse(&pack_bytes, format).expect("test operation should succeed");
        let expected_oids = roots.iter().copied().collect::<HashSet<_>>();
        let parsed_oids = parsed
            .entries
            .iter()
            .map(|entry| entry.entry.oid)
            .collect::<HashSet<_>>();
        assert_eq!(parsed.checksum, summary.checksum);
        assert_eq!(parsed_oids, expected_oids);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn index_raw_pack_returns_validated_pack_metadata() {
        let root = temp_root("sley-index-raw-pack");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, b"pack indexed\n");
        let commit_oid = graph[0].0;
        let expected = graph
            .iter()
            .map(|(oid, object)| (*oid, (object.object_type, object.body.len() as u64)))
            .collect::<HashMap<_, _>>();
        let pack = build_reachable_pack(&db, format, std::iter::once(commit_oid), &HashSet::new())
            .expect("test operation should succeed")
            .expect("reachable pack should be built");

        let indexed = index_raw_pack(&pack.pack, format).expect("test operation should succeed");
        let mut cursor = std::io::Cursor::new(pack.pack.clone());
        let streamed = index_raw_pack_from_reader(&mut cursor, format)
            .expect("streamed pack indexing should match in-memory indexing");
        assert_eq!(streamed, indexed);
        let pack_path = root.join("reachable.pack");
        fs::write(&pack_path, &pack.pack).expect("test operation should succeed");
        let file_indexed = index_raw_pack_file(&pack_path, format)
            .expect("file-backed pack indexing should match in-memory indexing");
        assert_eq!(file_indexed, indexed);

        assert_eq!(indexed.pack_id, pack.checksum);
        assert_eq!(indexed.index, pack.index);
        assert_eq!(indexed.objects.len(), 3);
        for object in indexed.objects {
            let (expected_type, expected_size) = expected
                .get(&object.oid)
                .copied()
                .expect("indexed object should be reachable");
            assert_eq!(object.object_type, expected_type);
            assert_eq!(object.size, expected_size);
            assert!(object.offset > 0);
        }
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn reachable_object_helpers_follow_tags_and_report_missing_objects() {
        let root = temp_root("sley-reachable-tags");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

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
                raw_body: None,
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
        let err = collect_reachable_object_ids(&db, format, std::iter::once(missing))
            .expect_err("missing traversal root should error");
        let kind = err.not_found_kind().expect("typed not found");
        assert_eq!(kind.object_id(), Some(missing));
        assert_eq!(
            kind.missing_object_context(),
            Some(MissingObjectContext::Traversal)
        );
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
        let source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
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
        let source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
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
            fn install_raw_pack_from_reader<R>(
                &self,
                reader: &mut R,
            ) -> Result<RawPackInstallResult>
            where
                R: Read,
            {
                let mut pack_bytes = Vec::new();
                reader.read_to_end(&mut pack_bytes)?;
                self.packs.borrow_mut().push(pack_bytes.to_vec());
                let object_ids = self.installed.borrow().clone();
                Ok(RawPackInstallResult { object_ids })
            }
        }

        let format = ObjectFormat::Sha1;
        let source = ObjectDatabase::new(format);
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
                    force_large_offset: false,
                },
                sley_pack::MultiPackIndexEntry {
                    oid: second_oid,
                    pack_int_id: 1,
                    offset: second_pack.entries[0].offset,
                    force_large_offset: false,
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
    fn file_database_finds_pack_added_after_registry_was_cached() {
        // Regression guard for the cached pack-directory registry: a pack written
        // after the registry was first cached (via a prior read) must still be
        // discovered by the same handle, because a miss triggers a re-scan.
        let root = temp_root("sley-file-odb-pack-added-late");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        // First pack + object; reading it populates the registry cache.
        let first = EncodedObject::new(ObjectType::Blob, b"first late\n".to_vec());
        let first_oid = first
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first))
            .expect("test operation should succeed");
        db.install_pack(&first_pack)
            .expect("test operation should succeed");
        assert_eq!(read_object_for_assert(&db, &first_oid), first);

        // A second object that the cached registry does not yet know about.
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
        // a re-scan, not be masked by the stale registry.
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
    fn object_presence_checker_finds_pack_added_after_registry_was_cached() {
        let root = temp_root("sley-presence-checker-pack-added-late");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let first = EncodedObject::new(ObjectType::Blob, b"checker first late\n".to_vec());
        let first_oid = first
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first))
            .expect("test operation should succeed");
        db.install_pack(&first_pack)
            .expect("test operation should succeed");

        let second = EncodedObject::new(ObjectType::Blob, b"checker second late\n".to_vec());
        let second_oid = second
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut checker = db.presence_checker();
        assert!(
            checker
                .contains(&first_oid)
                .expect("test operation should succeed")
        );
        assert!(
            !checker
                .contains(&second_oid)
                .expect("test operation should succeed")
        );

        let second_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&second))
            .expect("test operation should succeed");
        db.install_pack(&second_pack)
            .expect("test operation should succeed");

        assert!(
            checker
                .contains(&second_oid)
                .expect("test operation should succeed")
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_pack_registry_loads_indexes_lazily_and_refreshes_after_count_change() {
        let root = temp_root("sley-file-odb-pack-registry-refresh");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let first = EncodedObject::new(ObjectType::Blob, b"registry first\n".to_vec());
        let first_oid = first
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first))
            .expect("test operation should succeed");
        db.install_pack(&first_pack)
            .expect("test operation should succeed");

        let first_registry = db
            .cached_pack_registry(&pack_dir, false)
            .expect("test operation should succeed");
        assert_eq!(first_registry.fingerprint.idx_count, 1);
        assert_eq!(first_registry.fingerprint.pack_count, 1);
        assert_eq!(first_registry.packs.len(), 1);
        assert!(
            first_registry.packs[0]
                .index
                .lock()
                .expect("test operation should succeed")
                .is_none()
        );
        assert!(
            first_registry.packs[0]
                .data
                .lock()
                .expect("test operation should succeed")
                .is_none()
        );

        // Existence checks use the parsed index directly and do not load pack
        // bytes; a full read fills the registry-owned pack data handle.
        assert!(
            db.contains(&first_oid)
                .expect("test operation should succeed")
        );
        assert!(
            first_registry.packs[0]
                .index
                .lock()
                .expect("test operation should succeed")
                .is_some()
        );
        assert!(
            first_registry.packs[0]
                .data
                .lock()
                .expect("test operation should succeed")
                .is_none()
        );
        assert_eq!(read_object_for_assert(&db, &first_oid), first);
        assert!(
            first_registry.packs[0]
                .data
                .lock()
                .expect("test operation should succeed")
                .is_some()
        );

        let second = EncodedObject::new(ObjectType::Blob, b"registry second\n".to_vec());
        let second_oid = second
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let second_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&second))
            .expect("test operation should succeed");
        db.install_pack(&second_pack)
            .expect("test operation should succeed");

        let refreshed = db
            .cached_pack_registry(&pack_dir, true)
            .expect("test operation should succeed");
        assert!(!Arc::ptr_eq(&first_registry, &refreshed));
        assert_eq!(refreshed.fingerprint.idx_count, 2);
        assert_eq!(refreshed.fingerprint.pack_count, 2);
        assert_eq!(refreshed.packs.len(), 2);
        assert_eq!(read_object_for_assert(&db, &second_oid), second);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn file_database_pack_search_hint_rebuilds_after_pack_added() {
        // Regression guard for the recent-pack search hint: it is tied to the
        // cached pack registry, so a miss followed by a changed registry must not
        // hide newly-added packs.
        let root = temp_root("sley-file-odb-pack-lookup-added-late");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let first = EncodedObject::new(ObjectType::Blob, b"first lookup\n".to_vec());
        let second = EncodedObject::new(ObjectType::Blob, b"second lookup\n".to_vec());
        let third = EncodedObject::new(ObjectType::Blob, b"third lookup\n".to_vec());
        let first_oid = first
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let second_oid = second
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let third_oid = third
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first))
            .expect("test operation should succeed");
        let second_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&second))
            .expect("test operation should succeed");
        db.install_pack(&first_pack)
            .expect("test operation should succeed");
        db.install_pack(&second_pack)
            .expect("test operation should succeed");

        // With two packs, these reads establish a cached registry and pack hint.
        assert_eq!(read_object_for_assert(&db, &first_oid), first);
        assert_eq!(read_object_for_assert(&db, &second_oid), second);
        assert!(matches!(
            db.read_object(&third_oid),
            Err(GitError::NotFound(_))
        ));

        let third_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&third))
            .expect("test operation should succeed");
        db.install_pack(&third_pack)
            .expect("test operation should succeed");

        assert_eq!(read_object_for_assert(&db, &third_oid), third);
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

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(object.clone())
            .expect("test operation should succeed");
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn bundle_prerequisite_verification_reads_existing_objects() {
        let db = ObjectDatabase::new(ObjectFormat::Sha1);
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
        let writer = ObjectDatabase::new(ObjectFormat::Sha256);
        let object = EncodedObject::new(ObjectType::Blob, b"transport pack object\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .expect("test operation should succeed");

        let result = unpack_packfile_objects(&pack.pack, ObjectFormat::Sha256, &writer)
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
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

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
