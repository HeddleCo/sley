use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::{Decompress, FlushDecompress};
use parking_lot::RwLock;
use std::sync::Mutex;
use sley_core::{GitError, MissingObjectContext, ObjectFormat, ObjectId, Result};
use sley_formats::{Bundle, BundleReference};
use sley_object::{
    Commit, EncodedObject, ObjectType, Tag, TreeEntries, parse_framed_object,
    tree_entry_object_type,
};
use sley_pack::{
    MultiPackIndex, MultiPackIndexOidLookup, PackBitmapIndex, PackBitmapWriter, PackFile,
    PackIndex, PackIndexByteSource, PackIndexEntry, PackIndexViewData, PackInput,
    PackReverseIndex, PackStreamIndexBuild, PackWrite, PackWriteOptions, PackWriteSummary,
};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::{env, fs};

use crate::{
    grafted_parents, implied_empty_tree_object, unique_temp_path, with_missing_object_context,
    ObjectReader, ObjectWriter,
};

use crate::install::{
    PackInstallResult, RawPackInstallOptions, RawPackInstaller, RawPackInstallResult,
    ReachablePackFile, ReachablePackWriteSummary, REACHABLE_PACK_STREAMING_MIN_OBJECTS,
    write_pack_component, write_promisor_pack_sidecar,
};
use crate::pack::FileObjectDatabase;
use crate::registry::{read_incremental_midx_chain, repository_objects_dir};
use crate::loose::{LooseObjectStore, collect_loose_object_ids};
use crate::repack::pack_index_entries_match_writer;

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

pub fn collect_reachable_object_ids_tolerating_promised_missing<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    collect_reachable_object_ids_excluding_promised_missing(reader, format, starts, &HashSet::new())
}

pub fn collect_reachable_object_ids_tolerating_missing<R, I>(
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

pub(crate) fn collect_reachable_object_ids_excluding_promised_missing<R, I>(
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
pub(crate) struct ReachablePackObject {
    pub(crate) oid: ObjectId,
    pub(crate) object: Arc<EncodedObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReachablePackObjectMeta {
    pub(crate) oid: ObjectId,
    pub(crate) object_type: ObjectType,
    pub(crate) size: u64,
}

pub(crate) enum ReachablePackObjectsForWrite {
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

pub(crate) fn collect_reachable_pack_objects_for_write<R, I>(
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

pub(crate) fn sort_reachable_pack_metadata(metadata: &mut [ReachablePackObjectMeta]) {
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

pub(crate) fn pack_inputs(objects: &[ReachablePackObject]) -> Vec<PackInput<'_>> {
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
pub(crate) fn loose_object_ids(objects_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let oids = loose_object_id_set(objects_dir, format)?;
    let mut oids = oids.into_iter().collect::<Vec<_>>();
    oids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(oids)
}

pub(crate) fn loose_object_id_set(objects_dir: &Path, format: ObjectFormat) -> Result<HashSet<ObjectId>> {
    let mut oids = HashSet::new();
    collect_loose_object_ids(objects_dir, format, &mut oids)?;
    Ok(oids)
}

/// Absolute paths of every `*.pack` file directly inside `pack_dir`, sorted for
/// deterministic output.
pub(crate) fn existing_pack_files(pack_dir: &Path) -> Result<Vec<PathBuf>> {
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
pub(crate) fn prune_obsolete_pack_paths(
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
pub(crate) fn prune_stale_multi_pack_index(
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
pub(crate) fn prune_loose_objects<'a, I>(
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

pub(crate) enum PackDeltaBase {
    Offset(u64),
    Ref(ObjectId),
}

pub(crate) struct PackIndexOffsetInfo {
    pub(crate) end_offset: u64,
    pub(crate) delta_base_oid: Option<ObjectId>,
}

pub(crate) fn scan_pack_index_offsets(
    index: &PackIndexViewData,
    target_offset: u64,
    trailer_offset: Option<u64>,
    delta_base_offset: Option<u64>,
) -> Result<PackIndexOffsetInfo> {
    let mut target_count = 0usize;
    let mut next_offset = None;
    let mut delta_base_oid = None;

    for idx in 0..index.count {
        let Some(lookup) = index.lookup_at(idx) else {
            continue;
        };
        if lookup.offset == target_offset {
            target_count += 1;
        } else if lookup.offset > target_offset {
            match next_offset {
                Some(current) if current <= lookup.offset => {}
                _ => next_offset = Some(lookup.offset),
            }
        }
        if Some(lookup.offset) == delta_base_offset {
            delta_base_oid = Some(index.oid_at(idx)?);
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

pub(crate) fn scan_pack_offsets_without_index(
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

pub(crate) fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(crate) fn inflate_pack_member_len(compressed: &[u8]) -> Result<usize> {
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

pub(crate) fn pack_entry_delta_base(
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

pub(crate) fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}

/// Remove `path` if it exists, treating a missing file as success.
pub(crate) fn remove_file_if_exists(path: &Path) -> Result<()> {
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
