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

use crate::loose::{collect_loose_object_ids, LooseObjectStore};
use crate::pack::{FileObjectDatabase, PackData, PackBytesCache, PackHeaderTypeCache, PackHeaderTypeCaches, PackDeltaCaches, DecodedObjectCache, LruOffsetCache, delta_base_cache_budget, load_pack_index_data, load_pack_data, promisor_pack_object_ids};

#[derive(Debug)]
pub(crate) struct RegisteredPack {
    pub(crate) idx: PathBuf,
    pub(crate) pack: PathBuf,
    pub(crate) index: RwLock<Option<Arc<PackIndexViewData>>>,
    pub(crate) data: Mutex<Option<Arc<PackData>>>,
    pub(crate) delta_cache: Arc<Mutex<LruOffsetCache>>,
    pub(crate) header_type_cache: PackHeaderTypeCache,
}

impl RegisteredPack {
    pub(crate) fn new(idx: PathBuf, pack: PathBuf) -> Self {
        Self {
            idx,
            pack,
            index: RwLock::new(None),
            data: Mutex::new(None),
            delta_cache: Arc::new(Mutex::new(LruOffsetCache::new(delta_base_cache_budget()))),
            header_type_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn index(&self, format: ObjectFormat) -> Result<Arc<PackIndexViewData>> {
        if let Some(index) = self.index.read().as_ref()
        {
            return Ok(Arc::clone(index));
        }
        let index_bytes = load_pack_index_data(&self.idx)?;
        let index = Arc::new(PackIndexViewData::parse_trusted_source_without_checksum(
            index_bytes,
            format,
        )?);
        *self.index.write() = Some(Arc::clone(&index));
        Ok(index)
    }

    pub(crate) fn bytes(&self, pack_bytes: &PackBytesCache) -> Result<Arc<PackData>> {
        if let Ok(cache) = self.data.lock()
            && let Some(bytes) = cache.as_ref()
        {
            return Ok(Arc::clone(bytes));
        }
        if let Some(bytes) = pack_bytes.read().get(&self.pack)
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
        pack_bytes.write().insert(self.pack.clone(), Arc::clone(&bytes));
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackDirFingerprint {
    pub(crate) modified: Option<std::time::SystemTime>,
    pub(crate) idx_count: usize,
    pub(crate) pack_count: usize,
}

/// Snapshot of a pack directory's lookup state, shared across cloned handles.
/// New packs are still found: a lookup that misses every cached pack re-scans the
/// directory once before concluding the object is absent (see
/// [`FileObjectDatabase::find_pack_containing`]).
#[derive(Debug)]
pub(crate) struct PackRegistrySnapshot {
    pub(crate) fingerprint: PackDirFingerprint,
    pub(crate) packs: Vec<Arc<RegisteredPack>>,
    pub(crate) recent_pack: Mutex<Option<usize>>,
}

impl PackRegistrySnapshot {
    pub(crate) fn new(fingerprint: PackDirFingerprint, packs: Vec<Arc<RegisteredPack>>) -> Self {
        Self {
            fingerprint,
            packs,
            recent_pack: Mutex::new(None),
        }
    }

    pub(crate) fn cached_hint(&self) -> Option<usize> {
        self.recent_pack
            .lock()
            .ok()
            .and_then(|hint| *hint)
            .filter(|pack_index| *pack_index < self.packs.len())
    }

    pub(crate) fn remember_hint(&self, pack_index: usize) {
        if let Ok(mut hint) = self.recent_pack.lock() {
            *hint = Some(pack_index);
        }
    }
}

/// Cached pack-registry snapshot for this object directory, shared across cloned
/// handles. A `FileObjectDatabase` owns exactly one object directory, so this is
/// an `Option` instead of another path-keyed map.
pub(crate) type PackRegistryCache = Arc<Mutex<Option<Arc<PackRegistrySnapshot>>>>;

#[derive(Debug, Clone)]
pub(crate) struct PackLookup {
    pub(crate) pack: PathBuf,
    pub(crate) registered: Option<Arc<RegisteredPack>>,
    pub(crate) offset: u64,
}

impl PackLookup {
    pub(crate) fn from_registered(pack: Arc<RegisteredPack>, offset: u64) -> Self {
        Self {
            pack: pack.pack.clone(),
            registered: Some(pack),
            offset,
        }
    }

    pub(crate) fn from_path(pack: PathBuf, offset: u64) -> Self {
        Self {
            pack,
            registered: None,
            offset,
        }
    }

    pub(crate) fn pack_path(&self) -> &Path {
        &self.pack
    }

    pub(crate) fn pack_bytes(&self, database: &FileObjectDatabase) -> Result<Arc<PackData>> {
        match &self.registered {
            Some(pack) => pack.bytes(&database.pack_bytes),
            None => database.cached_pack_bytes(&self.pack),
        }
    }

    pub(crate) fn pack_index(&self, database: &FileObjectDatabase) -> Result<Arc<PackIndexViewData>> {
        match &self.registered {
            Some(pack) => pack.index(database.format),
            None => database.cached_pack_index(&self.pack.with_extension("idx")),
        }
    }

    pub(crate) fn delta_cache(&self, database: &FileObjectDatabase) -> Option<Arc<Mutex<LruOffsetCache>>> {
        match &self.registered {
            Some(pack) => Some(Arc::clone(&pack.delta_cache)),
            None => database.pack_delta_cache(&self.pack),
        }
    }

    pub(crate) fn header_type_cache(&self, database: &FileObjectDatabase) -> Option<PackHeaderTypeCache> {
        match &self.registered {
            Some(pack) => Some(Arc::clone(&pack.header_type_cache)),
            None => database.pack_header_type_cache(&self.pack),
        }
    }
}

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
    pub(crate) fn new(db: FileObjectDatabase) -> Self {
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

    pub(crate) fn find_packed(&mut self, oid: &ObjectId, force_rescan: bool) -> Result<bool> {
        self.prepare_packs(force_rescan)?;
        if let Some(midx) = &self.midx
            && midx.contains(oid)
        {
            return Ok(true);
        }
        self.prepare_registry(force_rescan)?;
        self.find_in_registry(oid)
    }

    pub(crate) fn prepare_packs(&mut self, force_rescan: bool) -> Result<()> {
        if self.prepared_packs && !force_rescan {
            return Ok(());
        }
        let midx_path = self.pack_dir.join("multi-pack-index");
        self.midx = self.db.cached_multi_pack_index_oid_lookup(&midx_path)?;
        self.prepared_packs = true;
        Ok(())
    }

    pub(crate) fn prepare_registry(&mut self, force_rescan: bool) -> Result<()> {
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

    pub(crate) fn find_in_registry(&mut self, oid: &ObjectId) -> Result<bool> {
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

    pub(crate) fn registry_index(
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

pub(crate) fn collect_packed_object_ids(
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

pub(crate) fn read_incremental_midx_chain(pack_dir: &Path) -> Result<Vec<String>> {
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

pub(crate) fn collect_incremental_midx_object_ids(
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

pub(crate) fn object_ids_with_prefix_in_objects_dir(
    objects_dir: &Path,
    format: ObjectFormat,
    prefix: &[u8],
) -> Result<Vec<ObjectId>> {
    let mut matches = HashSet::new();
    collect_loose_object_ids_with_prefix(objects_dir, format, prefix, &mut matches)?;
    collect_packed_object_ids_with_prefix(&objects_dir.join("pack"), format, prefix, &mut matches)?;
    let mut matches = matches.into_iter().collect::<Vec<_>>();
    matches.sort_by_key(ObjectId::to_hex);
    Ok(matches)
}

pub(crate) fn collect_loose_object_ids_with_prefix(
    objects_dir: &Path,
    format: ObjectFormat,
    prefix: &[u8],
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if prefix.len() < 2 {
        return Ok(());
    }
    let fanout_hex = std::str::from_utf8(&prefix[..2]).map_err(|_| {
        GitError::InvalidObjectId("object id prefix must be ASCII hex".into())
    })?;
    let fanout_dir = objects_dir.join(fanout_hex);
    let entries = match fs::read_dir(&fanout_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let hex_len = format.hex_len();
    for object_entry in entries {
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
        let oid = ObjectId::from_hex(format, &format!("{fanout_hex}{suffix}"))?;
        if oid.hex_prefix_matches(prefix) {
            oids.insert(oid);
        }
    }
    Ok(())
}

pub(crate) fn collect_packed_object_ids_with_prefix(
    pack_dir: &Path,
    format: ObjectFormat,
    prefix: &[u8],
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !pack_dir.exists() {
        return Ok(());
    }
    let floor = object_id_floor_for_hex_prefix(format, prefix)?;
    let mut midx_pack_names = HashSet::new();
    let midx_path = pack_dir.join("multi-pack-index");
    if midx_path.exists() {
        let midx = MultiPackIndex::parse_without_checksum(&fs::read(&midx_path)?, format)?;
        midx_pack_names.extend(midx.pack_names.iter().cloned());
        collect_multi_pack_index_prefix_matches(&midx, prefix, &floor, oids);
    }
    collect_incremental_midx_prefix_matches(pack_dir, format, prefix, &floor, oids)?;
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
        collect_pack_index_prefix_matches(&index, prefix, &floor, oids);
    }
    Ok(())
}

pub(crate) fn collect_incremental_midx_prefix_matches(
    pack_dir: &Path,
    format: ObjectFormat,
    prefix: &[u8],
    floor: &ObjectId,
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
        collect_multi_pack_index_prefix_matches(&midx, prefix, floor, oids);
    }
    Ok(())
}

pub(crate) fn collect_multi_pack_index_prefix_matches(
    midx: &MultiPackIndex,
    prefix: &[u8],
    floor: &ObjectId,
    oids: &mut HashSet<ObjectId>,
) {
    if midx.objects.is_empty() {
        return;
    }
    let (start, end) = pack_index_fanout_range(&midx.fanout, floor.as_bytes()[0]);
    if start >= end || end > midx.objects.len() {
        return;
    }
    let lower = lower_bound_pack_index_entries(
        &midx.objects,
        start,
        end,
        floor.as_bytes(),
        |entry| &entry.oid,
    );
    for entry in &midx.objects[lower..end] {
        if entry.oid.hex_prefix_matches(prefix) {
            oids.insert(entry.oid.clone());
        } else {
            break;
        }
    }
}

pub(crate) fn collect_pack_index_prefix_matches(
    index: &PackIndex,
    prefix: &[u8],
    floor: &ObjectId,
    oids: &mut HashSet<ObjectId>,
) {
    if index.entries.is_empty() {
        return;
    }
    let (start, end) = pack_index_fanout_range(&index.fanout, floor.as_bytes()[0]);
    if start >= end || end > index.entries.len() {
        return;
    }
    let lower = lower_bound_pack_index_entries(
        &index.entries,
        start,
        end,
        floor.as_bytes(),
        |entry| &entry.oid,
    );
    for entry in &index.entries[lower..end] {
        if entry.oid.hex_prefix_matches(prefix) {
            oids.insert(entry.oid.clone());
        } else {
            break;
        }
    }
}

pub(crate) fn pack_index_fanout_range(fanout: &[u32; 256], first_byte: u8) -> (usize, usize) {
    let bucket = usize::from(first_byte);
    let start = if bucket == 0 {
        0
    } else {
        fanout[bucket - 1] as usize
    };
    let end = fanout[bucket] as usize;
    (start, end)
}

pub(crate) fn lower_bound_pack_index_entries<T>(
    entries: &[T],
    start: usize,
    end: usize,
    floor: &[u8],
    oid: impl Fn(&T) -> &ObjectId,
) -> usize {
    let mut lo = start;
    let mut hi = end;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if oid(&entries[mid]).as_bytes() < floor {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

pub(crate) fn object_id_floor_for_hex_prefix(format: ObjectFormat, prefix: &[u8]) -> Result<ObjectId> {
    let mut hex = String::with_capacity(format.hex_len());
    for &byte in prefix {
        hex.push(char::from(byte.to_ascii_lowercase()));
    }
    while hex.len() < format.hex_len() {
        hex.push('0');
    }
    ObjectId::from_hex(format, &hex)
}

pub(crate) fn validate_object_id_prefix(format: ObjectFormat, prefix: &str) -> Result<()> {
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
pub(crate) fn scan_pack_registry(pack_dir: &Path, _format: ObjectFormat) -> Result<PackRegistrySnapshot> {
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
pub(crate) fn same_registered_pack_set(left: &[Arc<RegisteredPack>], right: &[Arc<RegisteredPack>]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(a, b)| a.idx == b.idx && a.pack == b.pack)
}

pub(crate) fn alternate_object_dirs(objects_dir: &Path) -> Vec<PathBuf> {
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
