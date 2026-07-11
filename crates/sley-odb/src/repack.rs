use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries};
use sley_pack::{PackFile, PackIndex, PackIndexEntry, PackInput};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{ObjectReader, grafted_parents};

use crate::install::{write_pack_component, write_promisor_pack_sidecar};
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
            bitmap_cache: bitmap_object_cache(&objects),
            loose_prune_outcome: LooseObjectPruneOutcome::FollowUpRequired,
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
    let database = if options.local {
        FileObjectDatabase::without_alternates(objects_dir.clone(), format)
    } else {
        FileObjectDatabase::new(objects_dir.clone(), format)
    };
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
