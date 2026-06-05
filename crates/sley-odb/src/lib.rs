use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::{Bundle, BundleReference};
use sley_object::{parse_framed_object, Commit, EncodedObject, ObjectType, Tag, Tree};
use sley_pack::{MultiPackIndex, PackFile, PackIndex, PackWrite};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::{env, fs};

static TEMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub trait ObjectReader {
    fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject>;
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
            Err(GitError::NotFound(_)) => missing.push(prerequisite.oid.clone()),
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
    Err(GitError::NotFound(format!(
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
    let mut seen = HashSet::new();
    let mut objects = Vec::new();
    for oid in starts {
        collect_reachable_object(
            reader,
            format,
            oid,
            &HashSet::new(),
            &mut seen,
            &mut objects,
        )?;
    }
    Ok(seen)
}

pub fn collect_reachable_objects<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<Vec<EncodedObject>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let mut seen = HashSet::new();
    let mut objects = Vec::new();
    for oid in starts {
        collect_reachable_object(reader, format, oid, excluded, &mut seen, &mut objects)?;
    }
    Ok(objects)
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
    let objects = collect_reachable_objects(reader, format, starts, excluded)?;
    if objects.is_empty() {
        return Ok(None);
    }
    // Delta-compress reachable packs (used by install/push/fetch) via git-pack's
    // sliding-window selection. Self-contained, ofs-delta by default; round-trips
    // through the existing parser. PackWrite shape is unchanged, so callers are
    // unaffected.
    PackFile::write_packed(&objects, format).map(Some)
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
        objects.push(database.read_object(oid)?);
    }

    let written = PackFile::write_packed(&objects, format)?;
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
    packed_loose.sort_by_key(ObjectId::to_hex);

    Ok(Some(RepackResult {
        pack: written.pack,
        idx: written.index,
        object_count,
        obsolete_packs,
        packed_loose,
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

    // Recompute the pack checksum from the written bytes rather than trusting
    // the caller-provided name; this keeps the on-disk file name canonical and
    // lets us re-derive the object set the new pack actually contains.
    let parsed_pack = PackFile::parse(&result.pack, format)?;
    let pack_name = format!("pack-{}", parsed_pack.checksum.to_hex());
    let new_pack_path = pack_dir.join(format!("{pack_name}.pack"));
    let new_index_path = pack_dir.join(format!("{pack_name}.idx"));
    write_pack_component(&new_pack_path, &result.pack)?;
    write_pack_component(&new_index_path, &result.idx)?;

    if !prune {
        return Ok(());
    }

    // The authoritative set of object ids that are now safely packed.
    let present: HashSet<ObjectId> = parsed_pack
        .entries
        .iter()
        .map(|entry| entry.entry.oid.clone())
        .collect();

    prune_packs_contained_in(&objects_dir, format, &present, &new_pack_path)?;
    prune_loose_objects(&objects_dir, format, result.packed_loose.iter(), &present)?;
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
    let objects_dir = repository_objects_dir(git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);
    let reachable = collect_reachable_object_ids(&database, format, roots)?;

    let store = LooseObjectStore::new(objects_dir.clone(), format);
    let mut pruned: Vec<ObjectId> = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .filter(|oid| !reachable.contains(oid))
        .collect();
    pruned.sort_by_key(ObjectId::to_hex);

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
    oids.sort_by_key(ObjectId::to_hex);
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
/// skipping `keep` (the pack just written). A stale multi-pack-index that only
/// references removed packs is removed too.
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
        // Every object in this pack is safely in the new pack: remove the pack,
        // its index, and any known sidecar files.
        remove_file_if_exists(&pack_path)?;
        remove_file_if_exists(&index_path)?;
        for ext in ["promisor", "keep", "rev", "mtimes", "bitmap"] {
            remove_file_if_exists(&pack_path.with_extension(ext))?;
        }
        removed_stems.insert(stem.to_string_lossy().into_owned());
    }

    prune_stale_multi_pack_index(&pack_dir, format, &removed_stems)?;
    Ok(())
}

/// Remove a `multi-pack-index` if every pack it names was removed (its stems
/// are all in `removed_stems`), preventing readers from following it to a
/// deleted pack.
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
    let all_referenced_removed = midx.pack_names.iter().all(|name| {
        let stem = name.strip_suffix(".idx").unwrap_or(name);
        removed_stems.contains(stem)
    });
    if all_referenced_removed {
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

/// Remove `path` if it exists, treating a missing file as success.
fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

fn collect_reachable_object<R>(
    reader: &R,
    format: ObjectFormat,
    oid: ObjectId,
    excluded: &HashSet<ObjectId>,
    seen: &mut HashSet<ObjectId>,
    objects: &mut Vec<EncodedObject>,
) -> Result<()>
where
    R: ObjectReader,
{
    if excluded.contains(&oid) {
        return Ok(());
    }
    if !seen.insert(oid.clone()) {
        return Ok(());
    }
    let object = reader.read_object(&oid)?;
    match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse(format, &object.body)?;
            objects.push(object);
            collect_reachable_object(reader, format, commit.tree, excluded, seen, objects)?;
            for parent in commit.parents {
                collect_reachable_object(reader, format, parent, excluded, seen, objects)?;
            }
        }
        ObjectType::Tree => {
            let tree = Tree::parse(format, &object.body)?;
            objects.push(object);
            for entry in tree.entries {
                collect_reachable_object(reader, format, entry.oid, excluded, seen, objects)?;
            }
        }
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body)?;
            objects.push(object);
            collect_reachable_object(reader, format, tag.object, excluded, seen, objects)?;
        }
        ObjectType::Blob => objects.push(object),
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ObjectDatabase {
    format: ObjectFormat,
    objects: HashMap<ObjectId, EncodedObject>,
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
    fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
        self.objects
            .get(oid)
            .cloned()
            .ok_or_else(|| GitError::NotFound(format!("object {oid}")))
    }
}

impl ObjectWriter for ObjectDatabase {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        self.objects.entry(oid.clone()).or_insert(object);
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
type PackBytesCache = Arc<Mutex<HashMap<PathBuf, Arc<Vec<u8>>>>>;

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
fn object_cache_budget() -> usize {
    cache_budget_from_env("SLEY_OBJECT_CACHE_BYTES", DEFAULT_OBJECT_CACHE_BYTES)
}

/// Approximate byte budget for each per-pack delta-base cache (see
/// [`DEFAULT_DELTA_BASE_CACHE_BYTES`], `SLEY_DELTA_BASE_CACHE_BYTES`).
fn delta_base_cache_budget() -> usize {
    cache_budget_from_env(
        "SLEY_DELTA_BASE_CACHE_BYTES",
        DEFAULT_DELTA_BASE_CACHE_BYTES,
    )
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
    map: HashMap<K, EncodedObject>,
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

    fn get(&mut self, key: &K) -> Option<EncodedObject> {
        let object = self.map.get(key)?.clone();
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

    fn put(&mut self, key: K, object: EncodedObject) {
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
    fn get(&self, offset: u64) -> Option<EncodedObject> {
        self.0.lock().ok()?.get(&offset)
    }

    fn insert(&self, offset: u64, object: &EncodedObject) {
        if let Ok(mut cache) = self.0.lock() {
            cache.put(offset, object.clone());
        }
    }
}

/// Parsed pack indexes keyed by `.idx` path, shared across cloned handles. Caches
/// the index parse so locating a packed object doesn't re-parse every `.idx` on
/// each read.
type PackIndexCache = Arc<Mutex<HashMap<PathBuf, Arc<PackIndex>>>>;

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
    object_ids_in_objects_dir(&repository_objects_dir(git_dir), format)
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
            pack_listing: Arc::new(Mutex::new(HashMap::new())),
            decoded: Arc::new(Mutex::new(LruObjectCache::new(object_cache_budget()))),
            pack_deltas: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        Self::new(repository_objects_dir(git_dir), format)
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
        let parsed_pack = PackFile::parse(&pack.pack, self.format)?;
        let parsed_index = PackIndex::parse(&pack.index, self.format)?;
        if parsed_pack.checksum != pack.checksum || parsed_index.pack_checksum != pack.checksum {
            return Err(GitError::InvalidFormat(
                "pack and index checksums do not match pack write".into(),
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
            object_ids: pack.entries.iter().map(|entry| entry.oid.clone()).collect(),
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
            object_ids: built
                .entries
                .iter()
                .map(|entry| entry.oid.clone())
                .collect(),
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

    fn read_packed_object(&self, oid: &ObjectId) -> Result<Option<EncodedObject>> {
        // Memory-capped decoded-object cache first (delta-base reuse for ref-delta
        // bases that resolve back through the store + repeated whole-object reads).
        if let Ok(mut cache) = self.decoded.lock() {
            if let Some(object) = cache.get(oid) {
                return Ok(Some(object));
            }
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
        let resolve_ref_base = |base: &ObjectId| self.read_object(base).ok();
        let object = match &delta_adapter {
            Some(adapter) => sley_pack::read_object_at_with_cache(
                &bytes,
                pack_paths.offset,
                self.format,
                resolve_ref_base,
                adapter,
            )?,
            None => {
                sley_pack::read_object_at(&bytes, pack_paths.offset, self.format, resolve_ref_base)?
            }
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
            cache.put(oid.clone(), object.clone());
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

    /// Raw bytes of the pack at `pack_path`, read at most once per database handle
    /// (cached, shared across clones). On a poisoned lock it falls back to reading
    /// without caching, preserving correctness.
    fn cached_pack_bytes(&self, pack_path: &Path) -> Result<Arc<Vec<u8>>> {
        if let Ok(cache) = self.pack_bytes.lock() {
            if let Some(bytes) = cache.get(pack_path) {
                return Ok(Arc::clone(bytes));
            }
        }
        let bytes = Arc::new(fs::read(pack_path)?);
        if let Ok(mut cache) = self.pack_bytes.lock() {
            cache.insert(pack_path.to_path_buf(), Arc::clone(&bytes));
        }
        Ok(bytes)
    }

    /// Parsed index for the `.idx` at `index_path`, parsed at most once per
    /// database handle. On a poisoned lock it falls back to parsing without
    /// caching, preserving correctness.
    fn cached_pack_index(&self, index_path: &Path) -> Result<Arc<PackIndex>> {
        if let Ok(cache) = self.pack_indexes.lock() {
            if let Some(index) = cache.get(index_path) {
                return Ok(Arc::clone(index));
            }
        }
        let index = Arc::new(PackIndex::parse(&fs::read(index_path)?, self.format)?);
        if let Ok(mut cache) = self.pack_indexes.lock() {
            cache.insert(index_path.to_path_buf(), Arc::clone(&index));
        }
        Ok(index)
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

    fn find_midx_pack_containing(
        &self,
        pack_dir: &Path,
        oid: &ObjectId,
    ) -> Result<Option<PackPaths>> {
        let midx_path = pack_dir.join("multi-pack-index");
        if !midx_path.exists() {
            return Ok(None);
        }
        let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, self.format)?;
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
            return Err(GitError::NotFound(format!(
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
    fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
        match self.loose.read_object(oid) {
            Ok(object) => return Ok(object),
            Err(GitError::Io(_)) | Err(GitError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
        if let Some(object) = self.read_packed_object(oid)? {
            return Ok(object);
        }
        for alternate in &self.alternates {
            match Self::without_alternates(alternate, self.format).read_object(oid) {
                Ok(object) => return Ok(object),
                Err(GitError::Io(_)) | Err(GitError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Err(GitError::NotFound(format!("object {oid}")))
    }
}

impl ObjectWriter for FileObjectDatabase {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId> {
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
}

impl ObjectReader for LooseObjectStore {
    fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
        let path = self.object_path(oid)?;
        if !path.exists() {
            return Err(GitError::NotFound(format!("object {oid}")));
        }
        let compressed = fs::read(&path)?;
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut framed = Vec::new();
        decoder.read_to_end(&mut framed)?;
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
        Ok(object)
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
    use sley_object::{Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntry};
    use sley_pack::PackFile;

    fn blob_of(byte: u8, len: usize) -> EncodedObject {
        EncodedObject::new(ObjectType::Blob, vec![byte; len])
    }

    #[test]
    fn lru_cache_evicts_by_byte_budget_least_recently_used_first() {
        // Budget holds two ~1 KiB objects but not three.
        let one = cached_object_cost(&blob_of(0, 1000));
        let mut cache = LruCache::<u32>::new(one * 2 + 8);
        cache.put(1, blob_of(b'a', 1000));
        cache.put(2, blob_of(b'b', 1000));
        // Touch key 1 so key 2 becomes least-recently-used.
        assert!(cache.get(&1).is_some());
        cache.put(3, blob_of(b'c', 1000));
        // Key 2 (LRU) is evicted; 1 and 3 remain.
        assert!(cache.get(&1).is_some());
        assert!(cache.get(&2).is_none());
        assert!(cache.get(&3).is_some());
    }

    #[test]
    fn lru_cache_zero_budget_is_inert() {
        let mut cache = LruCache::<u32>::new(0);
        cache.put(1, blob_of(b'a', 16));
        assert!(cache.get(&1).is_none());
    }

    #[test]
    fn lru_cache_skips_object_larger_than_budget_and_clears_stale_entry() {
        let mut cache = LruCache::<u32>::new(cached_object_cost(&blob_of(0, 100)));
        cache.put(1, blob_of(b'a', 50));
        assert!(cache.get(&1).is_some());
        // An object that cannot fit is not cached, and it evicts the prior entry
        // stored under the same key (so we never serve a stale value for it).
        cache.put(1, blob_of(b'b', 10_000));
        assert!(cache.get(&1).is_none());
        // A subsequent fitting insert under another key still works and accounting
        // is not corrupted by the oversized insert.
        cache.put(2, blob_of(b'c', 50));
        assert!(cache.get(&2).is_some());
    }

    #[test]
    fn lru_cache_replacing_entry_updates_byte_accounting() {
        // Budget holds two 500-byte objects (plus headroom) but not a 500 + a
        // ~1900-byte object.
        let small = cached_object_cost(&blob_of(0, 500));
        let mut cache = LruCache::<u32>::new(small * 2 + 200);
        cache.put(1, blob_of(b'a', 500));
        cache.put(2, blob_of(b'b', 500));
        assert!(cache.get(&1).is_some());
        assert!(cache.get(&2).is_some());
        // Replace key 2 (now MRU after the gets above re-ordered 1 then 2) with a
        // bigger value that still fits the budget alone but makes the running total
        // exceed it; the LRU (key 1) is evicted while the replaced key 2 stays.
        // This exercises the replace-path accounting.
        cache.put(2, blob_of(b'b', 1000));
        assert!(cache.get(&2).is_some());
        assert!(cache.get(&1).is_none());
    }

    #[test]
    fn write_and_validate_blob() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .unwrap();
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        db.validate(&oid).unwrap();
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
        let oid = store.write_object(object.clone()).unwrap();
        assert_eq!(store.read_object(&oid).unwrap(), object);
        assert!(store.object_path(&oid).unwrap().exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_reads_object_from_pack_index() {
        let root = temp_root("sley-file-odb-pack");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"packed\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .unwrap();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .unwrap();

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        assert!(db.contains(&oid).unwrap());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_resolves_unique_loose_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-loose");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"prefix loose\n".to_vec());
        let oid = db.write_object(object).unwrap();
        let prefix = &oid.to_hex()[..8];

        assert_eq!(
            db.resolve_prefix(prefix).unwrap(),
            ObjectPrefixResolution::Unique(oid.clone())
        );
        assert!(db.object_ids().unwrap().contains(&oid));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_resolves_unique_packed_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-packed");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"prefix packed\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        db.install_pack(&pack).unwrap();
        let prefix = &oid.to_hex()[..8];

        assert_eq!(
            db.resolve_prefix(prefix).unwrap(),
            ObjectPrefixResolution::Unique(oid)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_reports_ambiguous_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-ambiguous");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let mut seen = HashMap::new();
        let (prefix, first, second) = (0..10_000)
            .find_map(|idx| {
                let object =
                    EncodedObject::new(ObjectType::Blob, format!("ambiguous {idx}\n").into_bytes());
                let oid = db.write_object(object).unwrap();
                let prefix = oid.to_hex()[..4].to_string();
                if let Some(first) = seen.insert(prefix.clone(), oid.clone()) {
                    Some((prefix, first, oid))
                } else {
                    None
                }
            })
            .expect("test should find a 4-hex collision");

        let ObjectPrefixResolution::Ambiguous(mut matches) = db.resolve_prefix(&prefix).unwrap()
        else {
            panic!("expected ambiguous prefix {prefix}");
        };
        matches.sort_by_key(ObjectId::to_hex);
        let mut expected = vec![first, second];
        expected.sort_by_key(ObjectId::to_hex);
        assert_eq!(matches, expected);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_rejects_too_short_object_prefix() {
        let root = temp_root("sley-file-odb-prefix-short");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        assert!(matches!(
            db.resolve_prefix("abc"),
            Err(GitError::InvalidObjectId(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_reads_sha256_object_from_pack_index() {
        let root = temp_root("sley-file-odb-pack-sha256");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"packed sha256\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        let written =
            PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
                .unwrap();
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .unwrap();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .unwrap();

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);
        assert!(db.contains(&oid).unwrap());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_installs_sha256_pack_without_loose_objects() {
        let root = temp_root("sley-file-odb-install-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"installed sha256 pack\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);

        let result = db.install_pack(&pack).unwrap();

        assert_eq!(result.pack_name, format!("pack-{}", pack.checksum.to_hex()));
        assert_eq!(result.object_ids, vec![oid.clone()]);
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert_eq!(result.promisor_path, None);
        assert!(!db.loose().object_path(&oid).unwrap().exists());
        assert!(db.contains(&oid).unwrap());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_installs_raw_sha256_pack_without_loose_objects() {
        let root = temp_root("sley-file-odb-install-raw-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"installed raw sha256 pack\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);

        let result = db.install_raw_pack(&pack.pack).unwrap();

        assert_eq!(result.pack_name, format!("pack-{}", pack.checksum.to_hex()));
        assert_eq!(result.object_ids, vec![oid.clone()]);
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert_eq!(result.promisor_path, None);
        assert!(!db.loose().object_path(&oid).unwrap().exists());
        assert!(db.contains(&oid).unwrap());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_installs_raw_promisor_pack_with_sidecar() {
        let root = temp_root("sley-file-odb-install-raw-promisor-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"installed promisor pack\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack =
            PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1).unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let result = db
            .install_raw_pack_with_options(&pack.pack, RawPackInstallOptions { promisor: true })
            .unwrap();

        let promisor_path = result.promisor_path.expect("promisor sidecar");
        assert_eq!(promisor_path.file_stem(), result.pack_path.file_stem());
        assert_eq!(
            promisor_path.extension().and_then(|ext| ext.to_str()),
            Some("promisor")
        );
        assert!(promisor_path.exists());
        assert_eq!(fs::read(&promisor_path).unwrap(), b"");
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert!(!db.loose().object_path(&oid).unwrap().exists());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_objects_dir_uses_linked_worktree_common_dir() {
        let root = temp_root("sley-odb-common-dir");
        let common = root.join(".git");
        let admin = common.join("worktrees").join("linked");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();

        let common = fs::canonicalize(common).unwrap();
        assert_eq!(repository_common_dir(&admin), common);
        assert_eq!(repository_objects_dir(&admin), common.join("objects"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reachable_object_helpers_walk_graph_and_install_pack() {
        let root = temp_root("sley-reachable-pack");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).unwrap();
        fs::create_dir_all(destination_git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"reachable payload\n".to_vec());
        let blob_oid = source.write_object(blob.clone()).unwrap();
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: b"payload.txt".to_vec(),
                    oid: blob_oid.clone(),
                }],
            }
            .write(),
        );
        let tree_oid = source.write_object(tree.clone()).unwrap();
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid.clone(),
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"initial\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = source.write_object(commit.clone()).unwrap();

        let reachable =
            collect_reachable_object_ids(&source, format, std::iter::once(commit_oid.clone()))
                .unwrap();
        assert!(reachable.contains(&commit_oid));
        assert!(reachable.contains(&tree_oid));
        assert!(reachable.contains(&blob_oid));

        let install = install_reachable_pack(
            &source,
            &destination,
            format,
            std::iter::once(commit_oid.clone()),
        )
        .unwrap()
        .expect("reachable pack should be written");
        assert_eq!(install.object_ids.len(), 3);
        for (oid, object) in [
            (&commit_oid, &commit),
            (&tree_oid, &tree),
            (&blob_oid, &blob),
        ] {
            assert!(!destination.loose().object_path(oid).unwrap().exists());
            assert!(destination.contains(oid).unwrap());
            assert_eq!(destination.read_object(oid).unwrap(), *object);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reachable_object_helpers_respect_exclusions_and_duplicate_starts() {
        let root = temp_root("sley-reachable-exclusions");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"excluded payload\n".to_vec());
        let blob_oid = db.write_object(blob).unwrap();
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: b"payload.txt".to_vec(),
                    oid: blob_oid.clone(),
                }],
            }
            .write(),
        );
        let tree_oid = db.write_object(tree).unwrap();
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid.clone(),
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"initial\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = db.write_object(commit).unwrap();
        let excluded = HashSet::from([tree_oid.clone()]);

        let objects = collect_reachable_objects(
            &db,
            format,
            [commit_oid.clone(), commit_oid.clone()],
            &excluded,
        )
        .unwrap();

        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id(format).unwrap(), commit_oid);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn build_reachable_pack_returns_raw_pack_and_respects_empty_exclusions() {
        let root = temp_root("sley-build-reachable-pack");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let object = EncodedObject::new(ObjectType::Blob, b"raw reachable pack\n".to_vec());
        let oid = db.write_object(object.clone()).unwrap();
        let pack = build_reachable_pack(&db, format, std::iter::once(oid.clone()), &HashSet::new())
            .unwrap()
            .expect("reachable pack should be built");
        assert!(pack.pack.starts_with(b"PACK"));
        assert_eq!(pack.entries.len(), 1);
        assert_eq!(pack.entries[0].oid, oid.clone());

        let excluded = HashSet::from([oid]);
        assert!(build_reachable_pack(
            &db,
            format,
            pack.entries.into_iter().map(|entry| entry.oid),
            &excluded
        )
        .unwrap()
        .is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reachable_object_helpers_follow_tags_and_report_missing_objects() {
        let root = temp_root("sley-reachable-tags");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"tagged payload\n".to_vec());
        let blob_oid = db.write_object(blob).unwrap();
        let tag = EncodedObject::new(
            ObjectType::Tag,
            Tag {
                object: blob_oid.clone(),
                object_type: ObjectType::Blob,
                name: b"v1".to_vec(),
                tagger: Some(b"Example <example@example.invalid> 0 +0000".to_vec()),
                message: b"tag message\n".to_vec(),
            }
            .write(),
        );
        let tag_oid = db.write_object(tag).unwrap();

        let reachable =
            collect_reachable_object_ids(&db, format, std::iter::once(tag_oid.clone())).unwrap();
        assert!(reachable.contains(&tag_oid));
        assert!(reachable.contains(&blob_oid));

        let missing =
            ObjectId::from_hex(format, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(matches!(
            collect_reachable_object_ids(&db, format, std::iter::once(missing)),
            Err(GitError::NotFound(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_reachable_pack_empty_starts_create_no_pack() {
        let root = temp_root("sley-reachable-empty");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).unwrap();
        fs::create_dir_all(destination_git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);

        let result =
            install_reachable_pack(&source, &destination, format, Vec::<ObjectId>::new()).unwrap();

        assert!(result.is_none());
        assert!(!destination_git_dir.join("objects").join("pack").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_reachable_pack_excluding_skips_fully_excluded_starts() {
        let root = temp_root("sley-reachable-install-excluding");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).unwrap();
        fs::create_dir_all(destination_git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);
        let object = EncodedObject::new(ObjectType::Blob, b"excluded install\n".to_vec());
        let oid = source.write_object(object).unwrap();
        let excluded = HashSet::from([oid.clone()]);

        let result = install_reachable_pack_excluding(
            &source,
            &destination,
            format,
            std::iter::once(oid),
            &excluded,
        )
        .unwrap();

        assert!(result.is_none());
        assert!(!destination_git_dir.join("objects").join("pack").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_reachable_pack_supports_sha256() {
        let root = temp_root("sley-reachable-pack-sha256");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).unwrap();
        fs::create_dir_all(destination_git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha256;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);
        let object = EncodedObject::new(ObjectType::Blob, b"sha256 reachable pack\n".to_vec());
        let oid = source.write_object(object.clone()).unwrap();

        let pack = build_reachable_pack(
            &source,
            format,
            std::iter::once(oid.clone()),
            &HashSet::new(),
        )
        .unwrap()
        .expect("sha256 reachable pack should be built");
        assert!(pack.pack.starts_with(b"PACK"));
        assert_eq!(pack.entries[0].oid, oid);

        let result =
            install_reachable_pack(&source, &destination, format, std::iter::once(oid.clone()))
                .unwrap()
                .expect("sha256 reachable pack should be written");

        assert_eq!(result.object_ids, vec![oid.clone()]);
        assert!(!destination.loose().object_path(&oid).unwrap().exists());
        assert_eq!(destination.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
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
        let oid = source.write_object(object).unwrap();
        let installer = RecordingInstaller::default();
        installer.installed.borrow_mut().push(oid.clone());

        let result = install_reachable_pack(&source, &installer, format, std::iter::once(oid))
            .unwrap()
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
        fs::create_dir_all(&pack_dir).unwrap();
        let first = EncodedObject::new(ObjectType::Blob, b"first packed\n".to_vec());
        let second = EncodedObject::new(ObjectType::Blob, b"second packed\n".to_vec());
        let first_oid = first.object_id(ObjectFormat::Sha1).unwrap();
        let second_oid = second.object_id(ObjectFormat::Sha1).unwrap();
        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first)).unwrap();
        let second_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&second)).unwrap();
        let first_pack_name = format!("pack-{}.idx", first_pack.checksum.to_hex());
        let second_pack_name = format!("pack-{}.idx", second_pack.checksum.to_hex());
        fs::write(
            pack_dir.join(first_pack_name.replace(".idx", ".pack")),
            first_pack.pack,
        )
        .unwrap();
        fs::write(
            pack_dir.join(second_pack_name.replace(".idx", ".pack")),
            second_pack.pack,
        )
        .unwrap();
        let midx = MultiPackIndex::write(
            ObjectFormat::Sha1,
            2,
            &[first_pack_name, second_pack_name],
            &[
                sley_pack::MultiPackIndexEntry {
                    oid: first_oid.clone(),
                    pack_int_id: 0,
                    offset: first_pack.entries[0].offset,
                },
                sley_pack::MultiPackIndexEntry {
                    oid: second_oid.clone(),
                    pack_int_id: 1,
                    offset: second_pack.entries[0].offset,
                },
            ],
        )
        .unwrap();
        fs::write(pack_dir.join("multi-pack-index"), midx).unwrap();

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        assert!(db.contains(&second_oid).unwrap());
        assert_eq!(
            db.resolve_prefix(&second_oid.to_hex()[..8]).unwrap(),
            ObjectPrefixResolution::Unique(second_oid.clone())
        );
        assert_eq!(db.read_object(&second_oid).unwrap(), second);
        assert_eq!(db.read_object(&first_oid).unwrap(), first);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_finds_pack_added_after_listing_was_cached() {
        // Regression guard for the cached pack-directory listing: a pack written
        // after the listing was first cached (via a prior read) must still be
        // discovered by the same handle, because a miss triggers a re-scan.
        let root = temp_root("sley-file-odb-pack-added-late");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        // First pack + object; reading it populates the listing cache.
        let first = EncodedObject::new(ObjectType::Blob, b"first late\n".to_vec());
        let first_oid = first.object_id(ObjectFormat::Sha1).unwrap();
        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first)).unwrap();
        db.install_pack(&first_pack).unwrap();
        assert_eq!(db.read_object(&first_oid).unwrap(), first);

        // A second object that the cached listing does not yet know about.
        let second = EncodedObject::new(ObjectType::Blob, b"second late\n".to_vec());
        let second_oid = second.object_id(ObjectFormat::Sha1).unwrap();
        // It is genuinely absent right now.
        assert!(matches!(
            db.read_object(&second_oid),
            Err(GitError::NotFound(_))
        ));

        // Install its pack through the same handle; the next read must find it via
        // a re-scan, not be masked by the stale listing.
        let second_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&second)).unwrap();
        db.install_pack(&second_pack).unwrap();
        assert!(db.contains(&second_oid).unwrap());
        assert_eq!(db.read_object(&second_oid).unwrap(), second);
        // The original object still resolves too.
        assert_eq!(db.read_object(&first_oid).unwrap(), first);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_prefers_loose_object_over_packed_object() {
        let root = temp_root("sley-file-odb-prefer-loose");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"same\n".to_vec());
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .unwrap();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .unwrap();

        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = db.write_object(object.clone()).unwrap();
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundle_prerequisite_verification_reads_existing_objects() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"base\n".to_vec()))
            .unwrap();
        let bundle_bytes = format!("# v2 git bundle\n-{oid} base\n\n").into_bytes();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        verify_bundle_prerequisites(&bundle, &db).unwrap();
    }

    #[test]
    fn bundle_prerequisite_verification_reports_missing_objects() {
        let db = ObjectDatabase::new(ObjectFormat::Sha1);
        let missing =
            sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"missing\n").unwrap();
        let bundle_bytes = format!("# v2 git bundle\n-{missing} missing\n\n").into_bytes();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        assert!(verify_bundle_prerequisites(&bundle, &db).is_err());
    }

    #[test]
    fn unbundle_objects_writes_pack_entries_and_returns_refs() {
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"bundle object\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let bundle_bytes = format!("# v2 git bundle\n{oid} refs/heads/main\n\n")
            .into_bytes()
            .into_iter()
            .chain(pack.pack)
            .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        let result = unbundle_objects(&bundle, &prerequisite_reader, &mut writer).unwrap();
        assert_eq!(result.written_objects, vec![oid.clone()]);
        assert_eq!(result.references, bundle.references);
        assert_eq!(writer.read_object(&oid).unwrap(), object);
    }

    #[test]
    fn install_bundle_pack_writes_pack_and_returns_refs() {
        let root = temp_root("sley-install-bundle-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let database = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"bundle pack object\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let bundle_bytes = format!("# v2 git bundle\n{oid} refs/heads/main\n\n")
            .into_bytes()
            .into_iter()
            .chain(pack.pack)
            .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        let result = install_bundle_pack(&bundle, &prerequisite_reader, &database).unwrap();

        assert_eq!(result.written_objects, vec![oid.clone()]);
        assert_eq!(result.references, bundle.references);
        assert!(database.contains(&oid).unwrap());
        assert_eq!(database.read_object(&oid).unwrap(), object);
        assert!(!database.loose().object_path(&oid).unwrap().exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unpack_packfile_objects_writes_sha256_pack_entries() {
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha256);
        let object = EncodedObject::new(ObjectType::Blob, b"transport pack object\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .unwrap();

        let result =
            unpack_packfile_objects(&pack.pack, ObjectFormat::Sha256, &mut writer).unwrap();

        assert_eq!(result.written_objects, vec![oid.clone()]);
        assert_eq!(writer.read_object(&oid).unwrap(), object);
    }

    #[test]
    fn unbundle_objects_rejects_missing_prerequisites_before_writing() {
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha1);
        let missing =
            sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"missing\n").unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"bundle object\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let bundle_bytes =
            format!("# v2 git bundle\n-{missing} missing\n{oid} refs/heads/main\n\n")
                .into_bytes()
                .into_iter()
                .chain(pack.pack)
                .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        assert!(unbundle_objects(&bundle, &prerequisite_reader, &mut writer).is_err());
        assert!(!writer.contains(&oid));
    }

    /// Build a commit -> tree -> blob graph in `db`, returning the three object
    /// ids and their canonical encodings as `(oid, object)` pairs.
    fn write_commit_graph(
        db: &mut FileObjectDatabase,
        format: ObjectFormat,
        payload: &[u8],
    ) -> Vec<(ObjectId, EncodedObject)> {
        let blob = EncodedObject::new(ObjectType::Blob, payload.to_vec());
        let blob_oid = db.write_object(blob.clone()).unwrap();
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: b"payload.txt".to_vec(),
                    oid: blob_oid.clone(),
                }],
            }
            .write(),
        );
        let tree_oid = db.write_object(tree.clone()).unwrap();
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid.clone(),
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"initial\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = db.write_object(commit.clone()).unwrap();
        vec![(commit_oid, commit), (tree_oid, tree), (blob_oid, blob)]
    }

    fn repack_all_objects_consolidates_loose_and_pack(format: ObjectFormat) {
        let root = temp_root("sley-repack-all");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        // A pre-existing pack holds one blob; the rest of the graph is loose.
        let packed_blob = EncodedObject::new(ObjectType::Blob, b"already packed\n".to_vec());
        let packed_oid = packed_blob.object_id(format).unwrap();
        let existing_pack =
            PackFile::write_undeltified(std::slice::from_ref(&packed_blob), format).unwrap();
        let existing = db.install_pack(&existing_pack).unwrap();

        let graph = write_commit_graph(&mut db, format, b"repack payload\n");

        let mut expected: HashMap<ObjectId, EncodedObject> = graph.iter().cloned().collect();
        expected.insert(packed_oid.clone(), packed_blob.clone());

        let result = repack_all_objects(&git_dir, format)
            .unwrap()
            .expect("repository has objects");

        // The new pack round-trips and contains every original object byte-for-byte.
        assert_eq!(result.object_count, expected.len());
        let parsed = PackFile::parse(&result.pack, format).unwrap();
        assert_eq!(parsed.entries.len(), expected.len());
        for entry in &parsed.entries {
            let want = expected
                .get(&entry.entry.oid)
                .expect("packed object was in the repository");
            assert_eq!(&entry.object, want);
            assert_eq!(entry.object.object_id(format).unwrap(), entry.entry.oid);
        }
        // The generated index parses and agrees with the pack checksum.
        let idx = PackIndex::parse(&result.idx, format).unwrap();
        assert_eq!(idx.pack_checksum, parsed.checksum);
        assert_eq!(idx.entries.len(), expected.len());

        // The pre-existing pack is reported obsolete (by its .pack path).
        assert_eq!(result.obsolete_packs, vec![existing.pack_path.clone()]);
        // Every loose object id is reported as now packed.
        let mut want_loose: Vec<ObjectId> = graph.iter().map(|(oid, _)| oid.clone()).collect();
        want_loose.sort_by_key(ObjectId::to_hex);
        assert_eq!(result.packed_loose, want_loose);
        assert!(!result.packed_loose.contains(&packed_oid));

        fs::remove_dir_all(root).unwrap();
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
        fs::create_dir_all(git_dir.join("objects")).unwrap();

        assert!(repack_all_objects(&git_dir, ObjectFormat::Sha1)
            .unwrap()
            .is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_repack_result_writes_pack_without_pruning_by_default() {
        let root = temp_root("sley-repack-install-nodelete");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, format, b"install no prune\n");

        let result = repack_all_objects(&git_dir, format).unwrap().unwrap();
        install_repack_result(&git_dir, format, &result, false).unwrap();

        // New pack is on disk and readable.
        let parsed = PackFile::parse(&result.pack, format).unwrap();
        let pack_dir = git_dir.join("objects").join("pack");
        let pack_path = pack_dir.join(format!("pack-{}.pack", parsed.checksum.to_hex()));
        let idx_path = pack_dir.join(format!("pack-{}.idx", parsed.checksum.to_hex()));
        assert!(pack_path.exists());
        assert!(idx_path.exists());
        // Loose objects survive because prune was not requested.
        for (oid, object) in &graph {
            assert!(db.loose().object_path(oid).unwrap().exists());
            assert_eq!(db.read_object(oid).unwrap(), *object);
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_repack_result_prunes_obsolete_packs_and_loose_objects() {
        let root = temp_root("sley-repack-install-prune");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let packed_blob = EncodedObject::new(ObjectType::Blob, b"prune packed\n".to_vec());
        let existing_pack =
            PackFile::write_undeltified(std::slice::from_ref(&packed_blob), format).unwrap();
        let existing = db.install_pack(&existing_pack).unwrap();
        let graph = write_commit_graph(&mut db, format, b"prune payload\n");

        let result = repack_all_objects(&git_dir, format).unwrap().unwrap();
        let new_pack_checksum = PackFile::parse(&result.pack, format).unwrap().checksum;
        install_repack_result(&git_dir, format, &result, true).unwrap();

        // Obsolete pack and its index are gone.
        assert!(!existing.pack_path.exists());
        assert!(!existing.index_path.exists());
        // Packed loose objects are gone from disk.
        for (oid, _) in &graph {
            assert!(!db.loose().object_path(oid).unwrap().exists());
        }
        // The new consolidated pack remains and still serves every object.
        let pack_dir = git_dir.join("objects").join("pack");
        assert!(pack_dir
            .join(format!("pack-{}.pack", new_pack_checksum.to_hex()))
            .exists());
        let reopened = FileObjectDatabase::from_git_dir(&git_dir, format);
        for (oid, object) in &graph {
            assert!(reopened.contains(oid).unwrap());
            assert_eq!(reopened.read_object(oid).unwrap(), *object);
        }
        let packed_oid = packed_blob.object_id(format).unwrap();
        assert_eq!(reopened.read_object(&packed_oid).unwrap(), packed_blob);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_repack_result_keeps_loose_object_absent_from_new_pack() {
        // Safety: a loose object whose id is not in the new pack must survive
        // pruning even if the caller lists it in `packed_loose`.
        let root = temp_root("sley-repack-install-safety");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, format, b"safety packed\n");

        let mut result = repack_all_objects(&git_dir, format).unwrap().unwrap();

        // A loose object that is NOT in the new pack, but mislabeled as packed.
        let stray = EncodedObject::new(ObjectType::Blob, b"never packed\n".to_vec());
        let stray_oid = db.write_object(stray.clone()).unwrap();
        assert!(!result.packed_loose.contains(&stray_oid));
        result.packed_loose.push(stray_oid.clone());

        install_repack_result(&git_dir, format, &result, true).unwrap();

        // The stray loose object is untouched because it is not in the new pack.
        assert!(db.loose().object_path(&stray_oid).unwrap().exists());
        assert_eq!(db.read_object(&stray_oid).unwrap(), stray);
        // Genuinely packed loose objects were still removed.
        for (oid, _) in &graph {
            assert!(!db.loose().object_path(oid).unwrap().exists());
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_unreachable_loose_reports_and_deletes_only_unreachable() {
        let root = temp_root("sley-prune-unreachable");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, format, b"reachable payload\n");
        let commit_oid = graph[0].0.clone();

        // A dangling loose blob not referenced by the commit graph.
        let dangling = EncodedObject::new(ObjectType::Blob, b"dangling\n".to_vec());
        let dangling_oid = db.write_object(dangling).unwrap();

        // Report-only pass leaves everything on disk.
        let reported =
            prune_unreachable_loose(&git_dir, format, [commit_oid.clone()], false).unwrap();
        assert_eq!(reported, vec![dangling_oid.clone()]);
        assert!(db.loose().object_path(&dangling_oid).unwrap().exists());

        // Deleting pass removes only the unreachable object.
        let deleted =
            prune_unreachable_loose(&git_dir, format, [commit_oid.clone()], true).unwrap();
        assert_eq!(deleted, vec![dangling_oid.clone()]);
        assert!(!db.loose().object_path(&dangling_oid).unwrap().exists());
        for (oid, object) in &graph {
            assert!(db.loose().object_path(oid).unwrap().exists());
            assert_eq!(db.read_object(oid).unwrap(), *object);
        }

        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
