use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries};
use sley_pack::{PackFile, PackIndex, PackIndexEntry, PackInput, PackWriteOptions};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{ObjectReader, ObjectWriter, grafted_parents};

use crate::install::{replace_pack_component, write_pack_component, write_promisor_pack_sidecar};
use crate::loose::LooseObjectStore;
use crate::pack::FileObjectDatabase;
use crate::pack::promisor_pack_object_ids;
use crate::reachability::{
    BitmapPseudoMergeGroup, ReachabilityBitmapOptions, ReachablePackObject,
    build_pack_bitmap_with_cached_objects, build_pack_name_hash_cache,
    collect_reachable_object_ids_excluding_promised_missing, existing_pack_files, loose_object_ids,
    pack_inputs, prune_loose_objects, prune_obsolete_pack_paths, prune_stale_multi_pack_index,
    remove_file_if_exists,
};
use crate::registry::{
    alternate_object_dirs, collect_packed_object_ids, object_ids_in_objects_dir,
    repository_objects_dir,
};

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
    /// Object types plus shared commit/tree bodies retained from the repack
    /// walk until an optional bitmap is built. Blob bodies are deliberately not
    /// retained; bitmap construction only needs their type and pack position.
    bitmap_cache: Vec<RepackBitmapObject>,
    loose_prune_outcome: LooseObjectPruneOutcome,
}

#[derive(Debug, Clone)]
struct UnpackedObject {
    oid: ObjectId,
    object: Arc<EncodedObject>,
    mtime: u32,
}

/// Outcome for `repack -A`: a reachable pack plus packed-but-unreachable
/// objects that must become loose before obsolete source packs are removed.
#[derive(Debug, Clone)]
pub struct UnpackUnreachableRepackResult {
    pub repack: Option<RepackResult>,
    unpacked: Vec<UnpackedObject>,
    obsolete_packs: Vec<PathBuf>,
    retained_pack_stems: Vec<String>,
}

impl UnpackUnreachableRepackResult {
    pub fn unpacked_oids(&self) -> impl ExactSizeIterator<Item = ObjectId> + '_ {
        self.unpacked.iter().map(|entry| entry.oid)
    }
}

/// Whether installing a repack result accounts for every loose object that is
/// duplicated by a surviving pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LooseObjectPruneOutcome {
    /// [`install_repack_result`] has enough provenance to perform the complete
    /// packed-loose cleanup itself.
    Complete,
    /// Retained or promisor packs may duplicate loose objects outside the new
    /// pack, so the caller must run the broader packed-loose cleanup.
    FollowUpRequired,
}

impl RepackResult {
    /// Return the object type retained by the repack walk, when `oid` belongs
    /// to the result's packed closure.
    pub fn cached_object_type(&self, oid: &ObjectId) -> Option<ObjectType> {
        self.bitmap_cache
            .binary_search_by(|entry| entry.oid.as_bytes().cmp(oid.as_bytes()))
            .ok()
            .map(|index| self.bitmap_cache[index].object_type)
    }

    pub const fn loose_object_prune_outcome(&self) -> LooseObjectPruneOutcome {
        self.loose_prune_outcome
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepackBitmapObject {
    oid: ObjectId,
    object_type: ObjectType,
    graph_object: Option<Arc<EncodedObject>>,
}

fn bitmap_object_cache(objects: &[ReachablePackObject]) -> Vec<RepackBitmapObject> {
    let mut cache = objects
        .iter()
        .map(|entry| RepackBitmapObject {
            oid: entry.oid,
            object_type: entry.object.object_type,
            graph_object: matches!(
                entry.object.object_type,
                ObjectType::Commit | ObjectType::Tree
            )
            .then(|| Arc::clone(&entry.object)),
        })
        .collect::<Vec<_>>();
    cache.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
    cache
}

fn bitmap_lookup_cache(
    objects: &[RepackBitmapObject],
) -> (
    HashMap<ObjectId, ObjectType>,
    HashMap<ObjectId, Arc<EncodedObject>>,
) {
    let mut object_types = HashMap::with_capacity(objects.len());
    let mut graph_objects = HashMap::new();
    for entry in objects {
        object_types.insert(entry.oid, entry.object_type);
        if let Some(object) = &entry.graph_object {
            graph_objects.insert(entry.oid, Arc::clone(object));
        }
    }
    (object_types, graph_objects)
}

/// Reuse an existing single pack when the reachability walk proves that its
/// object set is already exactly the requested repack output. This is common
/// when `repack -adb` is repeated solely to rebuild bitmap selection metadata:
/// recompressing identical canonical objects cannot change the pack bytes, but
/// costs substantially more than reading the already-validated pack and index.
///
/// The exact set comparison is deliberately strict. Multiple packs, missing
/// index files, unreachable objects in the existing pack, or newly reachable
/// objects all fall back to the ordinary writer path.
fn reuse_exact_single_pack(
    objects_dir: &Path,
    format: ObjectFormat,
    objects: &[ReachablePackObject],
    retained_pack_stems: &[String],
) -> Result<Option<RepackResult>> {
    let pack_paths = existing_pack_files(&objects_dir.join("pack"))?;
    let [pack_path] = pack_paths.as_slice() else {
        return Ok(None);
    };
    let index_path = pack_path.with_extension("idx");
    let Ok(index_bytes) = fs::read(&index_path) else {
        return Ok(None);
    };
    let index = match PackIndex::parse(&index_bytes, format) {
        Ok(index) => index,
        Err(_) => return Ok(None),
    };
    if index.entries.len() != objects.len() {
        return Ok(None);
    }
    let reachable: HashSet<ObjectId> = objects.iter().map(|entry| entry.oid).collect();
    if index
        .entries
        .iter()
        .any(|entry| !reachable.contains(&entry.oid))
    {
        return Ok(None);
    }
    let pack = match fs::read(pack_path) {
        Ok(pack) => pack,
        Err(_) => return Ok(None),
    };
    validate_pack_checksum(&pack, format, &index.pack_checksum, "reused repack")?;

    let canonical_name = format!("pack-{}.pack", index.pack_checksum.to_hex());
    let obsolete_packs =
        if pack_path.file_name().and_then(|name| name.to_str()) != Some(&canonical_name) {
            vec![pack_path.clone()]
        } else {
            Vec::new()
        };
    let mut packed_loose = loose_object_ids(objects_dir, format)?
        .into_iter()
        .filter(|oid| reachable.contains(oid))
        .collect::<Vec<_>>();
    packed_loose.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    Ok(Some(RepackResult {
        pack,
        idx: index_bytes,
        object_count: index.entries.len(),
        obsolete_packs,
        packed_loose,
        retained_pack_stems: retained_pack_stems.to_vec(),
        promisor: false,
        pack_checksum: index.pack_checksum,
        index_entries: index.entries,
        bitmap_cache: bitmap_object_cache(objects),
        loose_prune_outcome: LooseObjectPruneOutcome::FollowUpRequired,
    }))
}

struct RepackBitmapReader<'a> {
    cached: &'a HashMap<ObjectId, Arc<EncodedObject>>,
    fallback: &'a FileObjectDatabase,
}

impl ObjectReader for RepackBitmapReader<'_> {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        self.cached
            .get(oid)
            .map(Arc::clone)
            .map_or_else(|| self.fallback.read_object(oid), Ok)
    }

    fn is_shallow_graft(&self, oid: &ObjectId) -> bool {
        self.fallback.is_shallow_graft(oid)
    }

    fn has_shallow_grafts(&self) -> bool {
        self.fallback.has_shallow_grafts()
    }

    fn is_promised_object(&self, oid: &ObjectId) -> bool {
        self.fallback.is_promised_object(oid)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RepackOptions {
    /// Do not borrow objects from alternates (`git repack --local`).
    pub local: bool,
    /// Force fresh object compression even when one existing pack already has
    /// the exact reachable set (`git repack -f` / `-F`).
    pub force_rewrite: bool,
    /// Repack objects that are already in `.keep` / `--keep-pack` packs.
    pub pack_kept_objects: bool,
    /// Explicit `--keep-pack=<name>` pack stems (`pack-<checksum>`).
    pub keep_pack_stems: HashSet<String>,
}

/// Installation policy for pack companions produced by a repack.
///
/// Repository configuration remains a caller concern; the ODB engine receives
/// the already-resolved reverse-index policy explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepackInstallOptions {
    pub prune: bool,
    pub write_reverse_index: bool,
    pub write_bitmap_lookup_table: bool,
    pub write_bitmap_hash_cache: bool,
}

impl RepackInstallOptions {
    pub const fn new(prune: bool) -> Self {
        Self {
            prune,
            write_reverse_index: true,
            write_bitmap_lookup_table: false,
            write_bitmap_hash_cache: true,
        }
    }

    pub const fn with_reverse_index(mut self, write_reverse_index: bool) -> Self {
        self.write_reverse_index = write_reverse_index;
        self
    }

    pub const fn with_bitmap_extensions(
        mut self,
        write_lookup_table: bool,
        write_hash_cache: bool,
    ) -> Self {
        self.write_bitmap_lookup_table = write_lookup_table;
        self.write_bitmap_hash_cache = write_hash_cache;
        self
    }
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
    repack_reachable_objects_with_filter_to(git_dir, format, roots, options, None)
}

/// Build the `-A` form of an all-object repack.
///
/// Reachable objects go into the replacement pack. Objects found only in packs
/// being replaced are materialized as loose objects, retaining their source
/// pack timestamp, before those packs are pruned. When `unpack_before` is set,
/// old unreachable objects are omitted; `recent_roots` and their dependency
/// closure are always retained (the engine input for `gc.recentObjectsHook`).
pub fn repack_reachable_objects_unpack_unreachable(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    options: &RepackOptions,
    unpack_before: Option<u32>,
    recent_roots: &[ObjectId],
) -> Result<UnpackUnreachableRepackResult> {
    let objects_dir = repository_objects_dir(git_dir);
    let pack_dir = objects_dir.join("pack");
    let database = if options.local {
        FileObjectDatabase::without_alternates(objects_dir.clone(), format)
    } else {
        FileObjectDatabase::new(objects_dir.clone(), format)
    };
    let retained_pack_stems = repack_retained_pack_stems(
        &pack_dir,
        &options.keep_pack_stems,
        !options.pack_kept_objects,
    )?;
    let retained = retained_pack_stems
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let obsolete_packs = existing_pack_files(&pack_dir)?
        .into_iter()
        .filter(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_none_or(|stem| !retained.contains(stem))
                && !path.with_extension("keep").exists()
                && !path.with_extension("promisor").exists()
        })
        .collect::<Vec<_>>();

    let reachable = collect_reachable_object_ids_excluding_promised_missing(
        &database,
        format,
        roots.iter().copied(),
        &promisor_pack_object_ids(&objects_dir, format)?,
    )?;
    let recent = if recent_roots.is_empty() {
        HashSet::new()
    } else {
        collect_reachable_object_ids_excluding_promised_missing(
            &database,
            format,
            recent_roots.iter().copied(),
            &HashSet::new(),
        )?
    };
    let loose = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .collect::<HashSet<_>>();
    let packed_mtimes = packed_object_mtimes_for_paths(&obsolete_packs, format)?;
    let mut unpacked = Vec::new();
    for (oid, mtime) in packed_mtimes {
        if reachable.contains(&oid) || loose.contains(&oid) {
            continue;
        }
        if unpack_before.is_some_and(|cutoff| mtime <= cutoff) && !recent.contains(&oid) {
            continue;
        }
        match database.read_object(&oid) {
            Ok(object) => unpacked.push(UnpackedObject { oid, object, mtime }),
            Err(GitError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    unpacked.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));

    let repack = repack_reachable_objects_with_options(git_dir, format, roots, options)?;
    Ok(UnpackUnreachableRepackResult {
        repack,
        unpacked,
        obsolete_packs,
        retained_pack_stems,
    })
}

fn packed_object_mtimes_for_paths(
    pack_paths: &[PathBuf],
    format: ObjectFormat,
) -> Result<HashMap<ObjectId, u32>> {
    let mut mtimes: HashMap<ObjectId, u32> = HashMap::new();
    for pack_path in pack_paths {
        let index = PackIndex::parse(&fs::read(pack_path.with_extension("idx"))?, format)?;
        let per_object = fs::read(pack_path.with_extension("mtimes"))
            .ok()
            .and_then(|bytes| {
                sley_pack::PackMtimes::parse(&bytes, format, index.entries.len())
                    .ok()
                    .map(|parsed| parsed.mtimes)
            });
        let pack_mtime = path_mtime_secs(pack_path);
        for (position, entry) in index.entries.iter().enumerate() {
            let mtime = per_object
                .as_ref()
                .and_then(|values| values.get(position).copied())
                .unwrap_or(pack_mtime);
            mtimes
                .entry(entry.oid)
                .and_modify(|existing| *existing = (*existing).max(mtime))
                .or_insert(mtime);
        }
    }
    Ok(mtimes)
}

/// Repack reachable objects while moving blobs at least `limit` bytes into a
/// separate pack named `<filter_to>-<checksum>.{pack,idx}`. This is the native
/// engine seam for `repack --filter=blob:limit=<n> --filter-to=<prefix>`.
pub fn repack_reachable_objects_with_blob_limit_to(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    options: &RepackOptions,
    limit: u64,
    filter_to: &Path,
) -> Result<Option<RepackResult>> {
    repack_reachable_objects_with_filter_to(
        git_dir,
        format,
        roots,
        options,
        Some((limit, filter_to)),
    )
}

fn repack_reachable_objects_with_filter_to(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    options: &RepackOptions,
    filter_to: Option<(u64, &Path)>,
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

    let mut filtered_out = Vec::new();
    if let Some((limit, _)) = filter_to {
        let wanted = roots.iter().copied().collect::<HashSet<_>>();
        objects.retain(|entry| {
            let omit = entry.object.object_type == ObjectType::Blob
                && !wanted.contains(&entry.oid)
                && entry.object.body.len() as u64 >= limit;
            if omit {
                filtered_out.push(entry.clone());
            }
            !omit
        });
    }

    if let Some((_, prefix)) = filter_to
        && !filtered_out.is_empty()
    {
        write_filtered_repack(&filtered_out, format, prefix)?;
    }

    if objects.is_empty() {
        return Ok(None);
    }

    if filter_to.is_none()
        && !options.force_rewrite
        && let Some(mut reused) =
            reuse_exact_single_pack(&objects_dir, format, &objects, &retained_pack_stems)?
    {
        if retained_pack_stems.is_empty() && promisor_oids.is_empty() {
            reused.loose_prune_outcome = LooseObjectPruneOutcome::Complete;
        }
        return Ok(Some(reused));
    }

    let inputs = pack_inputs(&objects);
    let written = if options.force_rewrite {
        // A forced repack must not reproduce an existing pack through the
        // writer's canonical reorder path after bypassing whole-pack reuse.
        // Preserve traversal order while still allowing fresh delta selection.
        let write_options = PackWriteOptions::new().with_reorder(false);
        PackFile::write_packed_with_known_ids_and_options(&inputs, format, &write_options)?
    } else {
        PackFile::write_packed_with_known_ids(&inputs, format)?
    };
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
    let loose_prune_outcome = if retained_pack_stems.is_empty() && promisor_oids.is_empty() {
        LooseObjectPruneOutcome::Complete
    } else {
        LooseObjectPruneOutcome::FollowUpRequired
    };
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
        bitmap_cache: bitmap_object_cache(&objects),
        loose_prune_outcome,
    }))
}

fn write_filtered_repack(
    objects: &[ReachablePackObject],
    format: ObjectFormat,
    prefix: &Path,
) -> Result<()> {
    let written = PackFile::write_packed_with_known_ids(&pack_inputs(objects), format)?;
    let component_path = |extension: &str| {
        let mut path = prefix.as_os_str().to_os_string();
        path.push(format!("-{}.{}", written.checksum.to_hex(), extension));
        PathBuf::from(path)
    };
    write_pack_component(&component_path("pack"), &written.pack)?;
    write_pack_component(&component_path("idx"), &written.index)?;
    Ok(())
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
        bitmap_cache: bitmap_object_cache(&objects),
        loose_prune_outcome: LooseObjectPruneOutcome::FollowUpRequired,
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
        bitmap_cache: bitmap_object_cache(&objects),
        loose_prune_outcome: LooseObjectPruneOutcome::FollowUpRequired,
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
    let mut packed_oids = HashSet::new();
    collect_packed_object_ids(&objects_dir.join("pack"), format, &mut packed_oids)?;
    let loose_oids = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .filter(|oid| !packed_oids.contains(oid))
        .collect();
    repack_selected_loose_objects(&database, format, loose_oids)
}

/// Pack only loose objects that belong to the reachability closure of
/// `roots`. This is the engine for incremental `git repack`: unreachable loose
/// objects remain loose so a later cruft repack can timestamp and collect them.
pub fn repack_reachable_loose_objects(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
) -> Result<Option<RepackResult>> {
    let objects_dir = repository_objects_dir(git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);
    let promisor_oids = promisor_pack_object_ids(&objects_dir, format)?;
    // Reflogs can legitimately retain object ids whose objects have already
    // been pruned. `git repack` ignores those stale roots while still treating
    // a missing dependency below a present root as corruption. Filter only the
    // initial roots here, then keep the ordinary strict closure walk.
    let mut present_roots = Vec::with_capacity(roots.len());
    for oid in roots {
        if promisor_oids.contains(oid) || database.contains(oid)? {
            present_roots.push(*oid);
        }
    }
    let reachable = collect_reachable_object_ids_excluding_promised_missing(
        &database,
        format,
        present_roots,
        &promisor_oids,
    )?;
    let mut packed_oids = HashSet::new();
    collect_packed_object_ids(&objects_dir.join("pack"), format, &mut packed_oids)?;
    let loose_oids = loose_object_ids(&objects_dir, format)?
        .into_iter()
        .filter(|oid| reachable.contains(oid) && !packed_oids.contains(oid))
        .collect::<Vec<_>>();
    repack_selected_loose_objects(&database, format, loose_oids)
}

fn repack_selected_loose_objects(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    loose_oids: Vec<ObjectId>,
) -> Result<Option<RepackResult>> {
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
        bitmap_cache: bitmap_object_cache(&objects),
        loose_prune_outcome: LooseObjectPruneOutcome::FollowUpRequired,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeometricRepackPlan {
    pub split: usize,
    pub pack_count: usize,
}

/// Additional object-selection policy for a geometric repack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GeometricRepackOptions {
    /// Follow references from selected objects into packs outside the
    /// geometric progression. This is Git's `--stdin-packs=follow` mode and
    /// lets a new non-cruft pack rescue objects which were once unreachable
    /// before a bitmap MIDX intentionally excludes cruft packs.
    pub follow_reachable: bool,
}

/// Exact pack table and preferred pack for a MIDX written by `git repack`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeometricRepackMidxSelection {
    pub pack_names: Vec<String>,
    pub preferred_pack_name: Option<String>,
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

pub fn geometric_repack_plan(
    git_dir: &Path,
    format: ObjectFormat,
    split_factor: u64,
    kept_pack_stems: &HashSet<String>,
) -> Result<GeometricRepackPlan> {
    let objects_dir = repository_objects_dir(git_dir);
    let packs: Vec<GeometryPack> = collect_geometry_packs(&objects_dir, format, kept_pack_stems)?
        .into_iter()
        .filter(|pack| !pack.is_promisor)
        .collect();
    Ok(GeometricRepackPlan {
        split: compute_geometry_split(&packs, split_factor),
        pack_count: packs.len(),
    })
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
    repack_geometric_with_options(
        git_dir,
        format,
        split_factor,
        kept_pack_stems,
        GeometricRepackOptions::default(),
    )
}

pub fn repack_geometric_with_options(
    git_dir: &Path,
    format: ObjectFormat,
    split_factor: u64,
    kept_pack_stems: &HashSet<String>,
    options: GeometricRepackOptions,
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

    if options.follow_reachable && !included.is_empty() {
        let mut follow_starts = included.clone();
        // Packs left above the split are excluded from the new pack, but a
        // non-MIDX pack is not known to be closed under reachability. Git's
        // `!pack` stdin marker follows references out of those objects while
        // still excluding the objects stored in the pack itself.
        for oid in &excluded_oids {
            let object = database.read_object(oid)?;
            match object.object_type {
                ObjectType::Commit => {
                    let commit = Commit::parse_ref(format, &object.body)?;
                    follow_starts.extend(commit.parents.iter().copied());
                    follow_starts.push(commit.tree);
                }
                ObjectType::Tree => {
                    for entry in TreeEntries::new(format, &object.body) {
                        let entry = entry?;
                        if !entry.is_gitlink() {
                            follow_starts.push(entry.oid);
                        }
                    }
                }
                ObjectType::Tag => {
                    follow_starts.push(Tag::parse_ref(format, &object.body)?.object);
                }
                ObjectType::Blob => {}
            }
        }
        let followed = collect_reachable_object_ids_excluding_promised_missing(
            &database,
            format,
            follow_starts,
            &excluded_oids,
        )?;
        for oid in followed {
            if seen.insert(oid) {
                included.push(oid);
            }
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
            bitmap_cache: bitmap_object_cache(&objects),
            loose_prune_outcome: LooseObjectPruneOutcome::FollowUpRequired,
        }),
        rolled_up_packs,
    })
}

/// Select the explicit pack list used by `repack --write-midx` after a
/// geometric repack.
///
/// Cruft packs are normally retained because they may be needed to close a
/// bitmap traversal. When `repack.midxMustContainCruft=false`, they can be
/// omitted after `follow_reachable` copied those dependencies into the new
/// reachable pack. An existing MIDX pack which is neither still present nor
/// rolled into the new pack is "unknown" and conservatively keeps cruft in the
/// replacement MIDX, matching `midx_has_unknown_packs()`.
pub fn geometric_repack_midx_selection(
    git_dir: &Path,
    geometric: &GeometricRepackResult,
    midx_must_contain_cruft: bool,
    existing_midx_pack_names: Option<&HashSet<String>>,
) -> Result<GeometricRepackMidxSelection> {
    let pack_dir = repository_objects_dir(git_dir).join("pack");
    let mut non_cruft = Vec::new();
    let mut cruft = Vec::new();
    if let Ok(entries) = fs::read_dir(&pack_dir) {
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("idx") {
                continue;
            }
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToString::to_string)
            else {
                continue;
            };
            if path.with_extension("mtimes").exists() {
                cruft.push(name);
            } else {
                non_cruft.push(name);
            }
        }
    }

    let non_cruft_set: HashSet<&str> = non_cruft.iter().map(String::as_str).collect();
    let rolled_up: HashSet<String> = geometric
        .rolled_up_packs
        .iter()
        .filter_map(|path| path.file_stem().and_then(|stem| stem.to_str()))
        .map(|stem| format!("{stem}.idx"))
        .collect();
    let existing_has_unknown_pack = existing_midx_pack_names.is_some_and(|existing| {
        existing.iter().any(|name| {
            !non_cruft_set.contains(name.as_str()) && !rolled_up.contains(name.as_str())
        })
    });
    let include_cruft = midx_must_contain_cruft
        || existing_has_unknown_pack
        || (geometric.result.is_none() && existing_midx_pack_names.is_none());

    let mut pack_names = non_cruft;
    if include_cruft {
        pack_names.extend(cruft);
    }
    pack_names.sort();

    let preferred_pack_name = geometric.result.as_ref().and_then(|result| {
        let name = format!("pack-{}.idx", result.pack_checksum);
        pack_names
            .iter()
            .any(|candidate| candidate == &name)
            .then_some(name)
    });
    Ok(GeometricRepackMidxSelection {
        pack_names,
        preferred_pack_name,
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

/// Install a `repack -A` outcome. Unreachable objects are written loose before
/// pruning their source packs, so readers never observe them missing.
pub fn install_repack_with_unpacked_unreachable(
    git_dir: &Path,
    format: ObjectFormat,
    result: &UnpackUnreachableRepackResult,
    prune: bool,
) -> Result<()> {
    let objects_dir = repository_objects_dir(git_dir);
    let loose = LooseObjectStore::new(objects_dir.clone(), format);
    for entry in &result.unpacked {
        let written = loose.write_object(entry.object.as_ref().clone())?;
        if written != entry.oid {
            return Err(GitError::InvalidObject(
                "unpacked unreachable object changed identity".into(),
            ));
        }
        let path = loose.object_path(&entry.oid)?;
        fs::OpenOptions::new().read(true).open(path)?.set_modified(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(entry.mtime)),
        )?;
    }

    if let Some(repack) = result.repack.as_ref() {
        install_repack_result(git_dir, format, repack, prune)?;
    } else if prune {
        let nonexistent = objects_dir.join("pack/.sley-no-replacement-pack");
        prune_obsolete_pack_paths(
            &objects_dir,
            format,
            &result.obsolete_packs,
            &nonexistent,
            &result.retained_pack_stems,
            false,
        )?;
    }
    Ok(())
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
    install_repack_result_with_bitmap_options(
        git_dir,
        format,
        result,
        RepackInstallOptions::new(prune),
        bitmap_tips,
        bitmap_pseudo_merge_groups,
    )
}

/// Install a repack using an explicit sidecar policy.
pub fn install_repack_result_with_bitmap_options(
    git_dir: &Path,
    format: ObjectFormat,
    result: &RepackResult,
    options: RepackInstallOptions,
    bitmap_tips: Option<&HashSet<ObjectId>>,
    bitmap_pseudo_merge_groups: Option<&[BitmapPseudoMergeGroup]>,
) -> Result<()> {
    let objects_dir = repository_objects_dir(git_dir);
    let pack_dir = objects_dir.join("pack");
    let shared_repository = sley_formats::SharedRepositoryPermissions::from_git_dir(git_dir);
    shared_repository.create_dir_all(&pack_dir)?;

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
    // Repack defaults to a reverse index. When the caller disables it through
    // configuration, skip the optional sidecar while retaining the same
    // pack-before-index visibility ordering.
    let reverse_index = if options.write_reverse_index {
        Some(sley_pack::PackReverseIndex::write(
            format,
            &sley_pack::pack_order_index_positions(&parsed_index.entries),
            &result.pack_checksum,
        )?)
    } else {
        None
    };
    write_pack_component(&new_pack_path, &result.pack)?;
    if let Some(reverse_index) = reverse_index {
        write_pack_component(&new_rev_path, &reverse_index)?;
    } else {
        remove_file_if_exists(&new_rev_path)?;
    }
    write_pack_component(&new_index_path, &result.idx)?;
    let new_promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, result.promisor)?;
    shared_repository.adjust_file(&new_pack_path)?;
    if new_rev_path.exists() {
        shared_repository.adjust_file(&new_rev_path)?;
    }
    shared_repository.adjust_file(&new_index_path)?;
    if let Some(path) = new_promisor_path.as_deref() {
        shared_repository.adjust_file(path)?;
    }

    if let Some(tips) = bitmap_tips {
        // Build before pruning: the closure walk reads objects through the
        // shared objects retained by the repack walk, falling back to the
        // pre-existing packs/loose store only for an external delta base.
        let database = FileObjectDatabase::new(objects_dir.clone(), format);
        let (object_types, graph_objects) = bitmap_lookup_cache(&result.bitmap_cache);
        let bitmap_reader = RepackBitmapReader {
            cached: &graph_objects,
            fallback: &database,
        };
        let name_hash_cache = if options.write_bitmap_hash_cache {
            Some(build_pack_name_hash_cache(
                &bitmap_reader,
                format,
                &result.index_entries,
                &object_types,
            )?)
        } else {
            None
        };
        if let Some(bitmap) = build_pack_bitmap_with_cached_objects(
            &bitmap_reader,
            format,
            &result.index_entries,
            &result.pack_checksum,
            tips,
            bitmap_pseudo_merge_groups.unwrap_or(&[]),
            &object_types,
            &ReachabilityBitmapOptions {
                write_lookup_table: options.write_bitmap_lookup_table,
                name_hash_cache,
                restrict_to_tips: false,
            },
        )? {
            // Unlike the pack/idx/rev (content-addressed by the pack
            // checksum), the bitmap depends on selection inputs (e.g.
            // pack.preferBitmapTips), so an existing file must be replaced —
            // write_pack_component's exists-skip would keep a stale selection.
            let bitmap_path = pack_dir.join(format!("{pack_name}.bitmap"));
            remove_file_if_exists(&bitmap_path)?;
            write_pack_component(&bitmap_path, &bitmap)?;
            shared_repository.adjust_file(&bitmap_path)?;
        }
    }

    if !options.prune {
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
        let (object_types, graph_objects) = bitmap_lookup_cache(&result.bitmap_cache);
        let bitmap_reader = RepackBitmapReader {
            cached: &graph_objects,
            fallback: &database,
        };
        if let Some(bitmap) = build_pack_bitmap_with_cached_objects(
            &bitmap_reader,
            format,
            &result.index_entries,
            &result.pack_checksum,
            tips,
            &[],
            &object_types,
            &ReachabilityBitmapOptions::default(),
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
    /// Writer provenance used to validate the public pack/index bytes without
    /// inflating a potentially large cruft pack again during installation.
    index_entries: Vec<PackIndexEntry>,
}

/// Pack-writing policy specific to the unreachable-object side of a cruft
/// repack. Keeping this separate from [`RepackOptions`] lets callers resolve
/// command/config precedence once while the engine applies the same policy to
/// both the surviving and expired cruft packs.
#[derive(Debug, Clone)]
pub struct CruftPackOptions {
    /// Target size for each cruft pack. A single oversized object is allowed.
    pub max_pack_size: Option<u64>,
    /// Repack existing cruft packs strictly smaller than this size while
    /// retaining larger cruft packs unchanged. Ignored for expiring repacks.
    pub combine_cruft_below_size: Option<u64>,
    /// Delta-search and compression policy used to encode cruft objects.
    pub pack_write: PackWriteOptions,
}

impl Default for CruftPackOptions {
    fn default() -> Self {
        Self {
            max_pack_size: None,
            combine_cruft_below_size: None,
            pack_write: PackWriteOptions::new(),
        }
    }
}

/// Outcome of `git repack --cruft`: the reachable pack (if any) plus the cruft
/// `.mtimes` pack of surviving unreachable objects.
#[derive(Debug, Clone)]
pub struct CruftRepackResult {
    /// The all-into-one reachable pack, or `None` when nothing is reachable.
    pub reachable: Option<RepackResult>,
    /// The cruft pack of unreachable objects, or `None` when there are none.
    pub cruft: Option<CruftPack>,
    /// Additional cruft packs produced by a size-limited repack. The first
    /// pack remains in `cruft` for API compatibility with single-pack callers.
    pub additional_cruft: Vec<CruftPack>,
    /// Objects removed by `--cruft-expiration`, encoded before source packs are
    /// pruned. Callers implementing `--expire-to` can install this pack after
    /// the main repack without reopening objects that no longer exist locally.
    pub expired: Option<CruftPack>,
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

/// Public wrapper over `build_cruft_pack` for the `--expire-to` limbo pack.
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
    Ok(
        build_cruft_packs(database, format, survivors, None, &PackWriteOptions::new())?
            .into_iter()
            .next(),
    )
}

fn build_cruft_packs(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    survivors: &HashMap<ObjectId, u32>,
    max_pack_size: Option<u64>,
    pack_write: &PackWriteOptions,
) -> Result<Vec<CruftPack>> {
    if survivors.is_empty() {
        return Ok(Vec::new());
    }
    let mut ordered: Vec<(ObjectId, u32)> = survivors.iter().map(|(o, m)| (*o, *m)).collect();
    ordered.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let mut objects = Vec::with_capacity(ordered.len());
    for (oid, mtime) in ordered {
        match database.read_object(&oid) {
            Ok(object) => objects.push((oid, mtime, object)),
            Err(GitError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
    }
    if objects.is_empty() {
        return Ok(Vec::new());
    }

    // Prepare the complete pack once. This is both the common no-split result
    // and the sizing oracle for a limited run: its index offsets expose the
    // actual compressed/deltified byte contribution of every entry. Unlike an
    // uncompressed-body estimate, this keeps highly compressible objects
    // together and accounts for the configured delta window.
    let complete = build_one_cruft_pack(&objects, format, pack_write)?;
    let Some(limit) = max_pack_size.filter(|limit| *limit > 0) else {
        return Ok(vec![complete]);
    };
    if objects.len() == 1 || (complete.pack.len() as u64) < limit {
        return Ok(vec![complete]);
    }

    let groups = partition_cruft_objects_by_encoded_size(objects, &complete, format, limit)?;
    let mut packs = Vec::with_capacity(groups.len());
    for group in groups {
        build_size_limited_cruft_group(group, format, pack_write, limit, &mut packs)?;
    }
    Ok(packs)
}

/// Build one or more self-contained cruft packs from per-object mtimes using
/// the same encoded-size-aware splitter as `repack --cruft`.
///
/// This is the typed engine seam for the `pack-objects --cruft` adapter: the
/// caller remains responsible only for selecting candidate objects and
/// installing/rendering each returned pack.
pub fn build_cruft_packs_from_mtimes(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    object_mtimes: &HashMap<ObjectId, u32>,
    options: &CruftPackOptions,
) -> Result<Vec<CruftPack>> {
    build_cruft_packs(
        database,
        format,
        object_mtimes,
        options.max_pack_size,
        &options.pack_write,
    )
}

/// Build the protocol-visible empty cruft pack emitted by `pack-objects` when
/// expiration removes every candidate. Repository-level repack callers omit
/// empty cruft output, but the plumbing command still prints a pack name and
/// writes an empty `.mtimes` sidecar.
pub fn build_empty_cruft_pack(
    format: ObjectFormat,
    pack_write: &PackWriteOptions,
) -> Result<CruftPack> {
    build_one_cruft_pack(&[], format, pack_write)
}

type CruftObject = (ObjectId, u32, Arc<EncodedObject>);

fn build_size_limited_cruft_group(
    objects: Vec<CruftObject>,
    format: ObjectFormat,
    pack_write: &PackWriteOptions,
    limit: u64,
    packs: &mut Vec<CruftPack>,
) -> Result<()> {
    let pack = build_one_cruft_pack(&objects, format, pack_write)?;
    if objects.len() == 1 || (pack.pack.len() as u64) < limit {
        packs.push(pack);
        return Ok(());
    }

    // Removing a delta base that landed in an earlier pack can enlarge a later
    // group. Repartition only that overflowing group from its newly encoded
    // bytes. The usual path writes the complete sizing pack once and each final
    // group once; repeated work is confined to groups whose delta plan changed
    // across the split.
    let groups = partition_cruft_objects_by_encoded_size(objects, &pack, format, limit)?;
    if groups.len() <= 1 {
        return Err(GitError::InvalidFormat(
            "size-limited cruft pack could not make partition progress".into(),
        ));
    }
    for group in groups {
        build_size_limited_cruft_group(group, format, pack_write, limit, packs)?;
    }
    Ok(())
}

fn partition_cruft_objects_by_encoded_size(
    objects: Vec<CruftObject>,
    pack: &CruftPack,
    format: ObjectFormat,
    limit: u64,
) -> Result<Vec<Vec<CruftObject>>> {
    let trailer_start = pack
        .pack
        .len()
        .checked_sub(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("cruft pack is shorter than its trailer".into()))?;
    let mut entries = pack.index_entries.iter().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.offset);
    let mut objects_by_oid = objects
        .into_iter()
        .map(|entry| (entry.0, entry))
        .collect::<HashMap<_, _>>();
    let fixed_bytes = 12_u64.saturating_add(format.raw_len() as u64);
    let mut current_size = fixed_bytes;
    let mut current = Vec::new();
    let mut groups = Vec::new();

    for (position, entry) in entries.iter().enumerate() {
        let end = entries
            .get(position + 1)
            .map(|next| next.offset)
            .unwrap_or(trailer_start as u64);
        let encoded_size = end.checked_sub(entry.offset).ok_or_else(|| {
            GitError::InvalidFormat("cruft pack index offsets are not monotonic".into())
        })?;
        let object = objects_by_oid.remove(&entry.oid).ok_or_else(|| {
            GitError::InvalidFormat("cruft pack index names an unknown object".into())
        })?;
        // Git starts a new pack when the next encoded entry plus the final hash
        // would reach the limit, except that the first object is always allowed
        // to exceed it. `current_size` already includes header and trailer.
        if !current.is_empty() && current_size.saturating_add(encoded_size) >= limit {
            groups.push(std::mem::take(&mut current));
            current_size = fixed_bytes;
        }
        current_size = current_size.saturating_add(encoded_size);
        current.push(object);
    }
    if !objects_by_oid.is_empty() {
        return Err(GitError::InvalidFormat(
            "cruft pack omitted objects from its index".into(),
        ));
    }
    if !current.is_empty() {
        groups.push(current);
    }
    Ok(groups)
}

fn build_one_cruft_pack(
    objects: &[CruftObject],
    format: ObjectFormat,
    pack_write: &PackWriteOptions,
) -> Result<CruftPack> {
    let inputs: Vec<PackInput<'_>> = objects
        .iter()
        .map(|(oid, _, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect();
    let written = PackFile::write_packed_with_known_ids_and_options(&inputs, format, pack_write)?;
    let mtime_by_oid = objects
        .iter()
        .map(|(oid, mtime, _)| (*oid, *mtime))
        .collect::<HashMap<_, _>>();

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
    Ok(CruftPack {
        pack: written.pack,
        idx: written.index,
        rev,
        mtimes,
        checksum: written.checksum,
        oids: cruft_oids,
        index_entries: written.entries,
    })
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
    repack_cruft_with_options_and_max_size(git_dir, format, roots, cruft_expiration, options, None)
}

pub fn repack_cruft_with_options_and_max_size(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    cruft_expiration: Option<u32>,
    options: &RepackOptions,
    max_pack_size: Option<u64>,
) -> Result<CruftRepackResult> {
    let cruft_options = CruftPackOptions {
        max_pack_size,
        ..CruftPackOptions::default()
    };
    repack_cruft_with_pack_options(
        git_dir,
        format,
        roots,
        cruft_expiration,
        options,
        &cruft_options,
    )
}

/// Run a cruft repack with an explicit, typed cruft-pack policy.
pub fn repack_cruft_with_pack_options(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    cruft_expiration: Option<u32>,
    options: &RepackOptions,
    cruft_options: &CruftPackOptions,
) -> Result<CruftRepackResult> {
    repack_cruft_with_pack_options_and_recent_roots(
        git_dir,
        format,
        roots,
        &[],
        cruft_expiration,
        options,
        cruft_options,
    )
}

/// Run a cruft repack while treating each explicit root and its closure as
/// recent. This additive entry point keeps the original
/// [`repack_cruft_with_pack_options`] signature stable for embedders.
pub fn repack_cruft_with_pack_options_and_recent_roots(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    recent_roots: &[ObjectId],
    cruft_expiration: Option<u32>,
    options: &RepackOptions,
    cruft_options: &CruftPackOptions,
) -> Result<CruftRepackResult> {
    let objects_dir = repository_objects_dir(git_dir);
    let database = if options.local {
        FileObjectDatabase::without_alternates(objects_dir.clone(), format)
    } else {
        FileObjectDatabase::new(objects_dir.clone(), format)
    };
    let pack_dir = objects_dir.join("pack");
    // Selective combination keeps large local cruft packs intact and only
    // feeds objects from smaller cruft packs (plus loose objects) to the new
    // cruft pack. Expiration deliberately disables this selection because all
    // unreachable objects must participate in rescue/expiry decisions.
    let retained_cruft_pack_stems = if cruft_expiration.is_none() {
        cruft_options
            .combine_cruft_below_size
            .filter(|limit| *limit > 0)
            .map(|limit| retained_cruft_pack_stems(&pack_dir, limit))
            .transpose()?
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let retained_cruft_oids = pack_oids_for_stems(&pack_dir, format, &retained_cruft_pack_stems)?;
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
    // A local repack must not borrow an alternate-only ref root. Filter those
    // roots before the strict closure walk; local roots still receive full
    // missing-object validation once admitted.
    let reachable_roots = if options.local {
        let mut local_roots = Vec::with_capacity(roots.len());
        for oid in roots {
            if database.contains(oid)? {
                local_roots.push(*oid);
            }
        }
        local_roots
    } else {
        roots.to_vec()
    };
    let mut reachable_ids = collect_reachable_object_ids_excluding_promised_missing(
        &database,
        format,
        reachable_roots,
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
                bitmap_cache: bitmap_object_cache(&objects),
                loose_prune_outcome: LooseObjectPruneOutcome::FollowUpRequired,
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
                && !retained_cruft_oids.contains(oid)
        })
        .collect();
    if options.local {
        let mut alternate_oids = HashSet::new();
        for alternate in alternate_object_dirs(&objects_dir) {
            alternate_oids.extend(object_ids_in_objects_dir(alternate, format)?);
        }
        survivors.retain(|oid, _| !alternate_oids.contains(oid));
    }

    // Expiration: rescue older objects reachable from a recent one, drop the rest.
    // Preserve the dropped set as encoded output before installation can prune
    // its only source packs. This keeps `--expire-to` an engine operation rather
    // than forcing callers to race source deletion with a second ODB walk.
    let mut expired = None;
    if let Some(expiration) = cruft_expiration {
        let before_expiration = survivors.clone();
        rescue_and_expire_cruft_objects(
            &database,
            format,
            &mut survivors,
            expiration,
            recent_roots,
        )?;
        let dropped = before_expiration
            .into_iter()
            .filter(|(oid, _)| !survivors.contains_key(oid))
            .collect::<HashMap<_, _>>();
        expired = build_cruft_packs(&database, format, &dropped, None, &cruft_options.pack_write)?
            .into_iter()
            .next();
    }

    let mut cruft_packs = build_cruft_packs(
        &database,
        format,
        &survivors,
        cruft_options.max_pack_size,
        &cruft_options.pack_write,
    )?;
    let cruft = if cruft_packs.is_empty() {
        None
    } else {
        Some(cruft_packs.remove(0))
    };

    // The packs the reachable+cruft packs supersede: every pre-existing
    // non-kept pack. Cruft packs are tracked separately.
    let mut obsolete_packs = Vec::new();
    let mut obsolete_cruft_packs = Vec::new();
    for pack_path in existing_pack_files(&pack_dir)? {
        if let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str())
            && (retained_pack_stems.iter().any(|retained| retained == stem)
                || retained_cruft_pack_stems
                    .iter()
                    .any(|retained| retained == stem))
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
        additional_cruft: cruft_packs,
        expired,
        obsolete_packs,
        obsolete_cruft_packs,
        retained_pack_stems,
    })
}

fn retained_cruft_pack_stems(pack_dir: &Path, combine_below_size: u64) -> Result<Vec<String>> {
    let mut retained = Vec::new();
    for pack_path in existing_pack_files(pack_dir)? {
        if !pack_path.with_extension("mtimes").exists()
            || fs::metadata(&pack_path)?.len() < combine_below_size
        {
            continue;
        }
        if let Some(stem) = pack_path.file_stem().and_then(|stem| stem.to_str()) {
            retained.push(stem.to_string());
        }
    }
    retained.sort();
    Ok(retained)
}

fn validate_cruft_pack(format: ObjectFormat, cruft: &CruftPack) -> Result<()> {
    validate_pack_checksum(&cruft.pack, format, &cruft.checksum, "cruft pack")?;
    let parsed_index = PackIndex::parse(&cruft.idx, format)?;
    if parsed_index.pack_checksum != cruft.checksum
        || !pack_index_entries_match_writer(&parsed_index.entries, &cruft.index_entries)
    {
        return Err(GitError::InvalidFormat(
            "cruft pack index does not match pack contents".into(),
        ));
    }

    let mut indexed_oids = parsed_index
        .entries
        .iter()
        .map(|entry| entry.oid)
        .collect::<Vec<_>>();
    indexed_oids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut declared_oids = cruft.oids.clone();
    declared_oids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    if declared_oids != indexed_oids {
        return Err(GitError::InvalidFormat(
            "cruft object list does not match pack index".into(),
        ));
    }

    let reverse =
        sley_pack::PackReverseIndex::parse(&cruft.rev, format, parsed_index.entries.len())?;
    if reverse.pack_checksum != cruft.checksum {
        return Err(GitError::InvalidFormat(
            "cruft reverse index does not match pack checksum".into(),
        ));
    }
    let mtimes = sley_pack::PackMtimes::parse(&cruft.mtimes, format, parsed_index.entries.len())?;
    if mtimes.pack_checksum != cruft.checksum {
        return Err(GitError::InvalidFormat(
            "cruft mtimes does not match pack checksum".into(),
        ));
    }
    Ok(())
}

fn validate_cruft_repack_result(format: ObjectFormat, result: &CruftRepackResult) -> Result<()> {
    if let Some(reachable) = result.reachable.as_ref() {
        validate_pack_checksum(
            &reachable.pack,
            format,
            &reachable.pack_checksum,
            "reachable repack",
        )?;
        let parsed_index = PackIndex::parse(&reachable.idx, format)?;
        if parsed_index.pack_checksum != reachable.pack_checksum
            || !pack_index_entries_match_writer(&parsed_index.entries, &reachable.index_entries)
        {
            return Err(GitError::InvalidFormat(
                "reachable repack index does not match pack contents".into(),
            ));
        }
    }
    for cruft in result
        .cruft
        .iter()
        .chain(result.additional_cruft.iter())
        .chain(result.expired.iter())
    {
        validate_cruft_pack(format, cruft)?;
    }
    Ok(())
}

/// Install an already-built cruft pack at a pack-file prefix such as
/// `expired.git/objects/pack/pack`.
///
/// The bytes are self-contained, so this remains valid after the source
/// repository's obsolete packs have been pruned.
pub fn install_cruft_pack_at_prefix(
    format: ObjectFormat,
    cruft: &CruftPack,
    prefix: &Path,
) -> Result<PathBuf> {
    validate_cruft_pack(format, cruft)?;
    install_cruft_pack_at_prefix_validated(cruft, prefix)
}

fn install_cruft_pack_at_prefix_validated(cruft: &CruftPack, prefix: &Path) -> Result<PathBuf> {
    let parent = prefix.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let base = prefix
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pack");
    let stem = format!("{base}-{}", cruft.checksum.to_hex());
    let pack_path = parent.join(format!("{stem}.pack"));
    write_pack_component(&pack_path, &cruft.pack)?;
    write_pack_component(&parent.join(format!("{stem}.rev")), &cruft.rev)?;
    replace_pack_component(&parent.join(format!("{stem}.mtimes")), &cruft.mtimes)?;
    write_pack_component(&parent.join(format!("{stem}.idx")), &cruft.idx)?;
    Ok(pack_path)
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
    recent_roots: &[ObjectId],
) -> Result<()> {
    let recent: Vec<ObjectId> = survivors
        .iter()
        .filter(|(_, mtime)| **mtime > expiration)
        .map(|(oid, _)| *oid)
        .collect();

    let mut keep: HashSet<ObjectId> = HashSet::new();
    let mut pending: Vec<ObjectId> = recent;
    pending.extend_from_slice(recent_roots);
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
    validate_cruft_repack_result(format, result)?;
    install_cruft_repack_result_validated(git_dir, format, result, prune)
}

/// Install an optional expired-object backup before committing the source
/// cruft repack. Every output is validated before either destination is
/// mutated; a destination failure therefore leaves all source packs intact.
pub fn install_cruft_repack_result_with_expire_to(
    git_dir: &Path,
    format: ObjectFormat,
    result: &CruftRepackResult,
    prune: bool,
    expire_to: Option<&Path>,
) -> Result<()> {
    validate_cruft_repack_result(format, result)?;
    if let (Some(prefix), Some(expired)) = (expire_to, result.expired.as_ref()) {
        install_cruft_pack_at_prefix_validated(expired, prefix)?;
    }
    install_cruft_repack_result_validated(git_dir, format, result, prune)
}

fn install_cruft_repack_result_validated(
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
    let new_cruft_names = result
        .cruft
        .iter()
        .chain(result.additional_cruft.iter())
        .map(|cruft| format!("pack-{}.pack", cruft.checksum.to_hex()))
        .collect::<HashSet<_>>();

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
    for cruft in result.cruft.iter().chain(result.additional_cruft.iter()) {
        let pack_name = format!("pack-{}", cruft.checksum.to_hex());
        write_pack_component(&pack_dir.join(format!("{pack_name}.pack")), &cruft.pack)?;
        write_pack_component(&pack_dir.join(format!("{pack_name}.rev")), &cruft.rev)?;
        // A cruft repack may produce byte-identical pack contents after one of
        // its objects was rewritten loose. The pack checksum (and therefore
        // component basename) stays the same, but the per-object mtime table
        // must be refreshed. The generic component writer intentionally reuses
        // content-addressed files, so replace this mutable sidecar explicitly.
        let mtimes_path = pack_dir.join(format!("{pack_name}.mtimes"));
        replace_pack_component(&mtimes_path, &cruft.mtimes)?;
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
    for cruft in result.cruft.iter().chain(result.additional_cruft.iter()) {
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
        if file_name == new_reachable_name.as_deref()
            || file_name.is_some_and(|name| new_cruft_names.contains(name))
        {
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

pub(crate) fn pack_index_entries_match_writer(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_objects_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let objects = std::env::temp_dir().join(format!(
            "sley-cruft-{label}-{}-{nonce}/objects",
            std::process::id()
        ));
        fs::create_dir_all(&objects).expect("create objects directory");
        objects
    }

    fn pseudo_random_bytes(len: usize, mut seed: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            bytes.push((seed >> 24) as u8);
        }
        bytes
    }

    #[test]
    fn geometric_follow_reachable_rescues_once_cruft_dependencies() {
        let objects_dir = test_objects_dir("geometric-follow-cruft");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let format = ObjectFormat::Sha1;
        let database = FileObjectDatabase::new(objects_dir.clone(), format);
        let tree = database
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("write tree");
        let identity = b"T <t@example.invalid> 1 +0000".to_vec();
        let parent = database
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                Commit {
                    tree,
                    parents: Vec::new(),
                    author: identity.clone(),
                    committer: identity.clone(),
                    encoding: None,
                    message: b"once cruft\n".to_vec(),
                }
                .write(),
            ))
            .expect("write parent commit");
        let child = database
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                Commit {
                    tree,
                    parents: vec![parent],
                    author: identity.clone(),
                    committer: identity,
                    encoding: None,
                    message: b"reachable again\n".to_vec(),
                }
                .write(),
            ))
            .expect("write child commit");
        let cruft = build_cruft_pack(
            &database,
            format,
            &HashMap::from([(tree, 1_u32), (parent, 1_u32)]),
        )
        .expect("build cruft pack")
        .expect("non-empty cruft pack");
        install_cruft_pack_at_prefix(format, &cruft, &objects_dir.join("pack/pack"))
            .expect("install cruft pack");
        fs::remove_file(database.loose().object_path(&tree).expect("tree path"))
            .expect("remove loose tree");
        fs::remove_file(database.loose().object_path(&parent).expect("parent path"))
            .expect("remove loose parent");

        let ordinary = repack_geometric(git_dir, format, 2, &HashSet::new())
            .expect("ordinary geometric repack")
            .result
            .expect("pack child");
        assert_eq!(ordinary.object_count, 1);
        assert_eq!(ordinary.index_entries[0].oid, child);

        let followed = repack_geometric_with_options(
            git_dir,
            format,
            2,
            &HashSet::new(),
            GeometricRepackOptions {
                follow_reachable: true,
            },
        )
        .expect("follow-reachable geometric repack")
        .result
        .expect("pack followed closure");
        let followed_oids = followed
            .index_entries
            .iter()
            .map(|entry| entry.oid)
            .collect::<HashSet<_>>();
        assert_eq!(followed_oids, HashSet::from([tree, parent, child]));
        fs::remove_dir_all(git_dir).ok();
    }

    #[test]
    fn geometric_midx_selection_excludes_cruft_unless_existing_midx_requires_it() {
        let objects_dir = test_objects_dir("geometric-midx-cruft");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let pack_dir = objects_dir.join("pack");
        fs::create_dir_all(&pack_dir).expect("create pack directory");
        let checksum = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("checksum");
        let reachable_name = format!("pack-{checksum}.idx");
        fs::write(pack_dir.join(&reachable_name), b"reachable index marker")
            .expect("write reachable marker");
        fs::write(pack_dir.join("pack-cruft.idx"), b"cruft index marker")
            .expect("write cruft marker");
        fs::write(pack_dir.join("pack-cruft.mtimes"), b"cruft marker")
            .expect("write mtimes marker");
        let geometric = GeometricRepackResult {
            result: Some(RepackResult {
                pack: Vec::new(),
                idx: Vec::new(),
                object_count: 0,
                obsolete_packs: Vec::new(),
                packed_loose: Vec::new(),
                retained_pack_stems: Vec::new(),
                promisor: false,
                pack_checksum: checksum,
                index_entries: Vec::new(),
                bitmap_cache: Vec::new(),
                loose_prune_outcome: LooseObjectPruneOutcome::Complete,
            }),
            rolled_up_packs: Vec::new(),
        };

        let excluded = geometric_repack_midx_selection(git_dir, &geometric, false, None)
            .expect("exclude optional cruft");
        assert_eq!(excluded.pack_names, vec![reachable_name.clone()]);
        assert_eq!(excluded.preferred_pack_name, Some(reachable_name.clone()));

        let existing = HashSet::from(["pack-cruft.idx".to_string()]);
        let retained = geometric_repack_midx_selection(git_dir, &geometric, false, Some(&existing))
            .expect("retain required cruft");
        assert_eq!(
            retained.pack_names,
            vec![reachable_name, "pack-cruft.idx".to_string()]
        );
        fs::remove_dir_all(git_dir).ok();
    }

    #[test]
    fn explicit_recent_root_rescues_old_cruft_closure() {
        let objects_dir = test_objects_dir("recent-root-rescue");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let format = ObjectFormat::Sha1;
        let database = FileObjectDatabase::new(objects_dir.clone(), format);
        let tree = database
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .expect("write tree");
        let identity = b"T <t@example.invalid> 1 +0000".to_vec();
        let commit = database
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                Commit {
                    tree,
                    parents: Vec::new(),
                    author: identity.clone(),
                    committer: identity,
                    encoding: None,
                    message: b"hook root\n".to_vec(),
                }
                .write(),
            ))
            .expect("write commit");
        let mut survivors = HashMap::from([(tree, 1_u32), (commit, 1_u32)]);

        rescue_and_expire_cruft_objects(&database, format, &mut survivors, 100, &[commit])
            .expect("rescue explicit hook root");

        assert_eq!(
            survivors.keys().copied().collect::<HashSet<_>>(),
            HashSet::from([tree, commit])
        );
        fs::remove_dir_all(git_dir).ok();
    }

    #[test]
    fn local_cruft_repack_excludes_oid_already_present_in_alternate() {
        let objects_dir = test_objects_dir("local-alternate-dedup");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let alternate_objects = test_objects_dir("local-alternate-source");
        let local = FileObjectDatabase::new(objects_dir.clone(), ObjectFormat::Sha1);
        let alternate = FileObjectDatabase::new(alternate_objects.clone(), ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"duplicate unreachable\n".to_vec());
        let oid = local
            .write_object(object.clone())
            .expect("write local copy");
        assert_eq!(
            alternate
                .write_object(object)
                .expect("write alternate copy"),
            oid
        );
        fs::create_dir_all(objects_dir.join("info")).expect("create alternates directory");
        fs::write(
            objects_dir.join("info/alternates"),
            format!("{}\n", alternate_objects.display()),
        )
        .expect("write alternates file");

        let result = repack_cruft_with_pack_options(
            git_dir,
            ObjectFormat::Sha1,
            &[],
            None,
            &RepackOptions {
                local: true,
                ..RepackOptions::default()
            },
            &CruftPackOptions::default(),
        )
        .expect("local cruft repack");

        assert!(result.cruft.is_none());
        assert!(result.additional_cruft.is_empty());
        fs::remove_dir_all(git_dir).ok();
        fs::remove_dir_all(alternate_objects.parent().expect("alternate root")).ok();
    }

    #[test]
    fn incremental_repack_skips_loose_copy_already_in_pack() {
        let objects_dir = test_objects_dir("incremental-unpacked-only");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let format = ObjectFormat::Sha1;
        let database = FileObjectDatabase::new(objects_dir.clone(), format);
        let packed_object = EncodedObject::new(ObjectType::Blob, b"already packed\n".to_vec());
        let packed_oid = database
            .write_object(packed_object.clone())
            .expect("write packed object loose copy");
        let pack = PackFile::write_packed(&[packed_object], format).expect("build source pack");
        database.install_pack(&pack).expect("install source pack");
        let new_oid = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"new reachable loose object\n".to_vec(),
            ))
            .expect("write new loose object");

        let result = repack_reachable_loose_objects(git_dir, format, &[packed_oid, new_oid])
            .expect("incremental repack")
            .expect("new loose pack");

        assert_eq!(result.object_count, 1);
        assert_eq!(result.index_entries[0].oid, new_oid);
        fs::remove_dir_all(git_dir).ok();
    }

    #[test]
    fn size_limited_cruft_build_uses_encoded_size_and_allows_one_oversized_object() {
        let objects_dir = test_objects_dir("size-limit");
        let database = FileObjectDatabase::new(objects_dir.clone(), ObjectFormat::Sha1);
        let first = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                vec![b'a'; 1024 * 1024],
            ))
            .expect("write first blob");
        let second = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                vec![b'b'; 1024 * 1024],
            ))
            .expect("write second blob");
        let survivors = HashMap::from([(first, 1_u32), (second, 2_u32)]);

        let compressible = build_cruft_packs(
            &database,
            ObjectFormat::Sha1,
            &survivors,
            Some(1024 * 1024),
            &PackWriteOptions::new(),
        )
        .expect("pack compressible cruft objects");
        assert_eq!(compressible.len(), 1);
        assert_eq!(compressible[0].oids.len(), 2);
        assert!(compressible[0].pack.len() < 1024 * 1024);

        let random_first = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                pseudo_random_bytes(700 * 1024, 11),
            ))
            .expect("write incompressible first blob");
        let random_second = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                pseudo_random_bytes(700 * 1024, 29),
            ))
            .expect("write incompressible second blob");
        let split = build_cruft_packs(
            &database,
            ObjectFormat::Sha1,
            &HashMap::from([(random_first, 3_u32), (random_second, 4_u32)]),
            Some(1024 * 1024),
            &PackWriteOptions::new(),
        )
        .expect("split incompressible cruft objects");
        assert_eq!(split.len(), 2);
        assert!(
            split
                .iter()
                .all(|pack| { pack.oids.len() == 1 && pack.pack.len() < 1024 * 1024 })
        );

        let oversized = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                pseudo_random_bytes(2 * 1024 * 1024, 47),
            ))
            .expect("write oversized blob");
        let oversized_pack = build_cruft_packs(
            &database,
            ObjectFormat::Sha1,
            &HashMap::from([(oversized, 5_u32)]),
            Some(1024 * 1024),
            &PackWriteOptions::new(),
        )
        .expect("pack one oversized cruft object");
        assert_eq!(oversized_pack.len(), 1);
        assert_eq!(oversized_pack[0].oids, vec![oversized]);
        assert!(oversized_pack[0].pack.len() > 1024 * 1024);
        fs::remove_dir_all(objects_dir.parent().expect("fixture root")).ok();
    }

    #[test]
    fn cruft_build_applies_pack_window_to_delta_selection() {
        let objects_dir = test_objects_dir("window");
        let database = FileObjectDatabase::new(objects_dir.clone(), ObjectFormat::Sha1);
        let mut seed = 0x1234_5678_u32;
        let mut base = Vec::with_capacity(128 * 1024);
        for _ in 0..128 * 1024 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            base.push((seed >> 24) as u8);
        }
        let mut survivors = HashMap::new();
        for variant in 0..4_u8 {
            let mut body = base.clone();
            body[..32].fill(variant);
            let oid = database
                .write_object(EncodedObject::new(ObjectType::Blob, body))
                .expect("write similar blob");
            survivors.insert(oid, u32::from(variant) + 1);
        }

        let without_window = build_cruft_packs(
            &database,
            ObjectFormat::Sha1,
            &survivors,
            None,
            &PackWriteOptions::new().with_window(0),
        )
        .expect("build cruft pack without delta candidates");
        let with_window = build_cruft_packs(
            &database,
            ObjectFormat::Sha1,
            &survivors,
            None,
            &PackWriteOptions::new().with_window(3),
        )
        .expect("build cruft pack with delta candidates");

        assert_eq!(without_window.len(), 1);
        assert_eq!(with_window.len(), 1);
        assert!(
            with_window[0].pack.len() < without_window[0].pack.len() / 2,
            "delta window should materially reduce similar-object pack size"
        );
        fs::remove_dir_all(objects_dir.parent().expect("fixture root")).ok();
    }

    #[test]
    fn selective_cruft_combination_retains_large_pack_and_rewrites_small_pack() {
        let objects_dir = test_objects_dir("combine-selection");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let database = FileObjectDatabase::new(objects_dir.clone(), ObjectFormat::Sha1);
        let small_oid = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                pseudo_random_bytes(32 * 1024, 1),
            ))
            .expect("write small blob");
        let large_oid = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                pseudo_random_bytes(128 * 1024, 2),
            ))
            .expect("write large blob");
        let small = build_cruft_pack(
            &database,
            ObjectFormat::Sha1,
            &HashMap::from([(small_oid, 1_u32)]),
        )
        .expect("build small cruft pack")
        .expect("small cruft output");
        let large = build_cruft_pack(
            &database,
            ObjectFormat::Sha1,
            &HashMap::from([(large_oid, 2_u32)]),
        )
        .expect("build large cruft pack")
        .expect("large cruft output");
        let prefix = objects_dir.join("pack/pack");
        let small_path = install_cruft_pack_at_prefix(ObjectFormat::Sha1, &small, &prefix)
            .expect("install small cruft pack");
        let large_path = install_cruft_pack_at_prefix(ObjectFormat::Sha1, &large, &prefix)
            .expect("install large cruft pack");
        let small_size = fs::metadata(&small_path).expect("small metadata").len();
        let large_size = fs::metadata(&large_path).expect("large metadata").len();
        assert!(small_size < large_size);
        let threshold = small_size + (large_size - small_size) / 2;

        let result = repack_cruft_with_pack_options(
            git_dir,
            ObjectFormat::Sha1,
            &[],
            None,
            &RepackOptions::default(),
            &CruftPackOptions {
                combine_cruft_below_size: Some(threshold),
                ..CruftPackOptions::default()
            },
        )
        .expect("selective cruft repack");

        assert_eq!(
            result.cruft.as_ref().map(|pack| pack.oids.as_slice()),
            Some([small_oid].as_slice())
        );
        assert!(result.obsolete_cruft_packs.contains(&small_path));
        assert!(!result.obsolete_cruft_packs.contains(&large_path));
        fs::remove_dir_all(git_dir).ok();
    }

    fn one_blob_cruft(objects_dir: &Path, body: &[u8]) -> CruftPack {
        let database = FileObjectDatabase::new(objects_dir.to_path_buf(), ObjectFormat::Sha1);
        let oid = database
            .write_object(EncodedObject::new(ObjectType::Blob, body.to_vec()))
            .expect("write cruft blob");
        build_cruft_pack(
            &database,
            ObjectFormat::Sha1,
            &HashMap::from([(oid, 123_u32)]),
        )
        .expect("build cruft output")
        .expect("non-empty cruft output")
    }

    #[test]
    fn cruft_prefix_installer_validates_every_component_before_mutation() {
        let objects_dir = test_objects_dir("prevalidate-prefix");
        let root = objects_dir.parent().expect("fixture root");
        let valid = one_blob_cruft(&objects_dir, b"validated cruft\n");
        let mut invalid = Vec::new();

        let mut bad_pack = valid.clone();
        bad_pack.pack[12] ^= 1;
        invalid.push(("pack", bad_pack));
        let mut bad_index = valid.clone();
        let last = bad_index.idx.len() - 1;
        bad_index.idx[last] ^= 1;
        invalid.push(("index", bad_index));
        let mut bad_reverse = valid.clone();
        let last = bad_reverse.rev.len() - 1;
        bad_reverse.rev[last] ^= 1;
        invalid.push(("reverse", bad_reverse));
        let mut bad_mtimes = valid.clone();
        let last = bad_mtimes.mtimes.len() - 1;
        bad_mtimes.mtimes[last] ^= 1;
        invalid.push(("mtimes", bad_mtimes));
        let mut bad_oids = valid.clone();
        bad_oids.oids.clear();
        invalid.push(("oids", bad_oids));

        for (label, cruft) in invalid {
            let destination = root.join(format!("invalid-{label}"));
            let error =
                install_cruft_pack_at_prefix(ObjectFormat::Sha1, &cruft, &destination.join("pack"))
                    .expect_err("invalid cruft output must be rejected");
            assert!(!error.to_string().is_empty());
            assert!(
                !destination.exists(),
                "validation must precede destination creation for {label}"
            );
        }
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn expire_to_failure_leaves_source_pack_intact() {
        let objects_dir = test_objects_dir("expire-to-order");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let pack_dir = objects_dir.join("pack");
        fs::create_dir_all(&pack_dir).expect("create source pack directory");
        let obsolete = pack_dir.join("pack-obsolete.pack");
        fs::write(&obsolete, b"source must survive destination failure")
            .expect("write source marker");
        let cruft = one_blob_cruft(&objects_dir, b"expired backup\n");
        let result = CruftRepackResult {
            reachable: None,
            cruft: None,
            additional_cruft: Vec::new(),
            expired: Some(cruft),
            obsolete_packs: vec![obsolete.clone()],
            obsolete_cruft_packs: Vec::new(),
            retained_pack_stems: Vec::new(),
        };
        let blocked_parent = git_dir.join("blocked");
        fs::write(&blocked_parent, b"not a directory").expect("write blocked destination");

        install_cruft_repack_result_with_expire_to(
            git_dir,
            ObjectFormat::Sha1,
            &result,
            true,
            Some(&blocked_parent.join("pack")),
        )
        .expect_err("destination failure must abort before source prune");
        assert!(obsolete.is_file());
        fs::remove_dir_all(git_dir).ok();
    }

    #[test]
    fn invalid_cruft_result_does_not_prune_source_pack() {
        let objects_dir = test_objects_dir("prevalidate-source-prune");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let pack_dir = objects_dir.join("pack");
        fs::create_dir_all(&pack_dir).expect("create source pack directory");
        let obsolete = pack_dir.join("pack-obsolete.pack");
        fs::write(&obsolete, b"source marker").expect("write source marker");
        let mut cruft = one_blob_cruft(&objects_dir, b"invalid replacement\n");
        cruft.rev.pop();
        let result = CruftRepackResult {
            reachable: None,
            cruft: Some(cruft),
            additional_cruft: Vec::new(),
            expired: None,
            obsolete_packs: vec![obsolete.clone()],
            obsolete_cruft_packs: Vec::new(),
            retained_pack_stems: Vec::new(),
        };

        install_cruft_repack_result(git_dir, ObjectFormat::Sha1, &result, true)
            .expect_err("invalid replacement must abort before prune");
        assert!(obsolete.is_file());
        fs::remove_dir_all(git_dir).ok();
    }

    #[test]
    fn incremental_repack_ignores_only_missing_roots() {
        let objects_dir = test_objects_dir("incremental-stale-root");
        let git_dir = objects_dir.parent().expect("fixture git directory");
        let database = FileObjectDatabase::new(objects_dir.clone(), ObjectFormat::Sha1);
        let present = database
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"reachable loose\n".to_vec(),
            ))
            .expect("write reachable loose object");
        let missing = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ffffffffffffffffffffffffffffffffffffffff",
        )
        .expect("missing root oid");

        let result =
            repack_reachable_loose_objects(git_dir, ObjectFormat::Sha1, &[missing, present])
                .expect("stale roots are ignored")
                .expect("reachable loose pack");
        assert_eq!(result.object_count, 1);
        assert_eq!(result.index_entries[0].oid, present);
        fs::remove_dir_all(git_dir).ok();
    }

    #[test]
    fn bitmap_cache_does_not_retain_blob_bodies() {
        let blob = Arc::new(EncodedObject::new(
            ObjectType::Blob,
            vec![b'x'; 1024 * 1024],
        ));
        let oid = blob.object_id(ObjectFormat::Sha1).expect("blob id");
        let objects = vec![ReachablePackObject {
            oid,
            object: Arc::clone(&blob),
        }];

        let cache = bitmap_object_cache(&objects);

        assert_eq!(
            Arc::strong_count(&blob),
            2,
            "only the input and caller retain the blob"
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[0].object_type, ObjectType::Blob);
        assert!(cache[0].graph_object.is_none());
    }

    #[test]
    fn repack_result_exposes_cached_types_after_oid_sorting() {
        let blob = Arc::new(EncodedObject::new(ObjectType::Blob, b"blob".to_vec()));
        let commit = Arc::new(EncodedObject::new(ObjectType::Commit, b"commit".to_vec()));
        let blob_oid = blob.object_id(ObjectFormat::Sha1).expect("blob id");
        let commit_oid = commit.object_id(ObjectFormat::Sha1).expect("commit id");
        let objects = vec![
            ReachablePackObject {
                oid: blob_oid,
                object: blob,
            },
            ReachablePackObject {
                oid: commit_oid,
                object: commit,
            },
        ];
        let result = RepackResult {
            pack: Vec::new(),
            idx: Vec::new(),
            object_count: objects.len(),
            obsolete_packs: Vec::new(),
            packed_loose: Vec::new(),
            retained_pack_stems: Vec::new(),
            promisor: false,
            pack_checksum: blob_oid,
            index_entries: Vec::new(),
            bitmap_cache: bitmap_object_cache(&objects),
            loose_prune_outcome: LooseObjectPruneOutcome::Complete,
        };

        assert_eq!(result.cached_object_type(&blob_oid), Some(ObjectType::Blob));
        assert_eq!(
            result.cached_object_type(&commit_oid),
            Some(ObjectType::Commit)
        );
    }
}
