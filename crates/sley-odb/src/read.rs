//! Object reads on an explicit heap work stack.
//!
//! A pack entry, a packed copy of an object, and an object lookup are all
//! frames of one [`resolve_delta_chain`] walk. A REF-delta base becomes a
//! nested object lookup frame instead of a recursive `read_object` call, so
//! stack use stays constant however long a chain is and however many packs,
//! loose files, or alternates it crosses. Only the outermost lookup runs
//! outside the walk, so a plain packed read pushes no lookup frames.
//!
//! Each stage below mirrors one step of the store's source order: the
//! registry-selected pack copy, then loose storage, a re-selected pack, the
//! alternates and a refreshed loose probe; or, when a packed copy is corrupt,
//! every redundant copy before the original error is reported.

use crate::pack::{LruOffsetCache, PackData, PackDeltaCacheAdapter, verify_reads_enabled};
use crate::registry::PackLookup;
use crate::{FileObjectDatabase, ObjectReader, implied_empty_tree_object};
use sley_core::{GitError, MissingObjectContext, ObjectId, Result};
use sley_object::EncodedObject;
use sley_pack::chain::{DeltaChainBase, DeltaChainResolver, DeltaChainStep, resolve_delta_chain};
use sley_pack::{DecodedPackEntry, DeltaBase, PackDeltaCache, PackIndex, read_pack_entry_at};
use smallvec::SmallVec;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Read `oid` from `db` (no replacement mapping).
pub(crate) fn read_object(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
    let mut reader = Reader::new(db);
    // Cached, loose, and implied objects resolve here without a work stack.
    let (mut next, stage) = match reader.start(LOCAL, *oid)? {
        Start::Resolved(object) => return Ok(object),
        Start::Attempt(location, stage) => (location, stage),
    };
    // The top-level lookup is driven here rather than as a frame: most reads
    // decode one packed copy. Bases, including REF bases that come back
    // through a lookup, are frames of the walk inside `resolve_copy`.
    let mut lookup = Lookup {
        store: LOCAL,
        oid: *oid,
        stage,
    };
    // `start` has just probed the decoded-object cache for the first copy.
    let mut cache_checked = true;
    loop {
        let error = match reader.resolve_copy(next, cache_checked) {
            Ok(object) => return Ok(object),
            Err(error) => error,
        };
        match reader.recover_lookup(&mut lookup, error)? {
            Next::Resolved(object) => return Ok(object),
            Next::Attempt(location) => next = location,
        }
        cache_checked = false;
    }
}

/// The store being read, as opposed to one of its alternates.
const LOCAL: usize = 0;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Location {
    /// Look `oid` up in a store, trying every source in order.
    Object(usize, ObjectId),
    /// Decode the packed copy of `oid` at an offset in a source, verifying
    /// and caching the result as an ordinary packed object read.
    Copy {
        source: usize,
        offset: u64,
        oid: ObjectId,
    },
    /// Decode one pack entry and its delta base.
    Entry(usize, u64),
    /// Resume a queued lookup (redundant copies of an OFS base).
    Queued(usize),
}

enum Stage {
    /// The registry-selected pack copy is being read.
    Primary { source: usize },
    /// A pack re-selected after the first copy reported not-found; its errors
    /// are final.
    Reselected,
    /// No pack copy: alternates are being read, then loose storage is
    /// re-probed. `loose_error` is the earlier loose failure, if any.
    Alternates {
        remaining: VecDeque<Location>,
        loose_error: Option<GitError>,
    },
    /// A copy was corrupt: try every redundant copy, ignoring their errors,
    /// then report `error`. Other packs are found by a lazy directory scan,
    /// then the alternates are opened.
    Redundant {
        packs: Option<PackScan>,
        alternates: Option<VecDeque<Location>>,
        error: GitError,
    },
}

struct Lookup {
    store: usize,
    oid: ObjectId,
    stage: Stage,
}

enum Entry {
    Lookup(Lookup),
    Copy {
        store: usize,
        oid: ObjectId,
    },
    Packed {
        source: usize,
        offset: u64,
        entry: DecodedPackEntry,
        recovered: bool,
    },
}

/// A lazy scan of a store's pack directory for redundant copies. Like a
/// plain directory walk, it stops at the first usable copy, so a later
/// unreadable entry cannot fail a read that an earlier copy satisfies.
struct PackScan {
    entries: std::fs::ReadDir,
    exclude: Option<PathBuf>,
}

/// One pack file in one store, opened lazily.
struct Source {
    store: usize,
    lookup: PackLookup,
    bytes: Option<Arc<PackData>>,
    cache: Option<Arc<Mutex<LruOffsetCache>>>,
}

/// Where a lookup goes next.
enum Next {
    Resolved(Arc<EncodedObject>),
    Attempt(Location),
}

/// How a lookup begins: already resolved, or a first copy to try in a stage.
enum Start {
    Resolved(Arc<EncodedObject>),
    Attempt(Location, Stage),
}

struct Reader<'a> {
    db: &'a FileObjectDatabase,
    /// Alternate stores, opened on first use; store `n` is `alternates[n - 1]`.
    alternates: Vec<Option<FileObjectDatabase>>,
    /// Packs touched by this read; almost always one or two.
    sources: SmallVec<[Source; 2]>,
    /// Lookups queued for OFS-base recovery.
    queued: Vec<Option<Lookup>>,
}

impl<'a> Reader<'a> {
    fn new(db: &'a FileObjectDatabase) -> Self {
        Self {
            db,
            alternates: Vec::new(),
            sources: SmallVec::new(),
            queued: Vec::new(),
        }
    }

    fn store(&self, store: usize) -> &FileObjectDatabase {
        match store.checked_sub(1) {
            Some(index) => self
                .alternates
                .get(index)
                .and_then(Option::as_ref)
                .unwrap_or(self.db),
            None => self.db,
        }
    }

    /// Locations of `oid` in each alternate of `store`. Alternates carry no
    /// alternates of their own, matching the store's single-level lookup.
    fn alternate_locations(&mut self, store: usize, oid: ObjectId) -> VecDeque<Location> {
        if store != LOCAL {
            return VecDeque::new();
        }
        let count = self.db.alternates.len();
        if self.alternates.len() < count {
            self.alternates.resize_with(count, || None);
        }
        (0..count)
            .map(|index| {
                if self.alternates[index].is_none() {
                    self.alternates[index] = Some(FileObjectDatabase::without_alternates(
                        &self.db.alternates[index],
                        self.db.format,
                    ));
                }
                Location::Object(index + 1, oid)
            })
            .collect()
    }

    fn source(&mut self, store: usize, lookup: PackLookup) -> Location {
        let offset = lookup.offset;
        let existing = self
            .sources
            .iter()
            .position(|source| source.store == store && source.lookup.pack == lookup.pack);
        let source = match existing {
            Some(source) => source,
            None => {
                let cache = lookup.delta_cache(self.store(store));
                self.sources.push(Source {
                    store,
                    lookup,
                    bytes: None,
                    cache,
                });
                self.sources.len() - 1
            }
        };
        Location::Entry(source, offset)
    }

    fn copy(&mut self, store: usize, oid: ObjectId, lookup: PackLookup) -> Location {
        match self.source(store, lookup) {
            Location::Entry(source, offset) => Location::Copy {
                source,
                offset,
                oid,
            },
            location => location,
        }
    }

    fn cached(&self, store: usize, oid: &ObjectId) -> Option<Arc<EncodedObject>> {
        self.store(store).decoded.lock().ok()?.get(oid)
    }

    /// The next copy of `oid` in `scan`, reading `.idx` files directly so
    /// the registry, whose first hit is the excluded pack, is bypassed.
    /// Unreadable or unparsable indexes are skipped; a directory read error
    /// is reported.
    fn next_pack_copy(
        &mut self,
        store: usize,
        oid: ObjectId,
        scan: &mut PackScan,
    ) -> Result<Option<Location>> {
        let format = self.store(store).format;
        for entry in scan.entries.by_ref() {
            let idx_path = entry?.path();
            if idx_path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let pack_path = idx_path.with_extension("pack");
            if scan.exclude.as_ref() == Some(&pack_path) {
                continue;
            }
            let Ok(idx_bytes) = std::fs::read(&idx_path) else {
                continue;
            };
            let Ok(index) = PackIndex::parse(&idx_bytes, format) else {
                continue;
            };
            if let Some(entry) = index.find(&oid) {
                let lookup = PackLookup::from_path(pack_path, entry.offset);
                return Ok(Some(self.copy(store, oid, lookup)));
            }
        }
        Ok(None)
    }

    /// Redundant copies of `oid` outside source `exclude`: other packs of
    /// `store`, then its alternates.
    fn redundant_stage(&self, store: usize, exclude: usize, error: GitError) -> Stage {
        let packs = std::fs::read_dir(self.store(store).objects_dir.join("pack"))
            .ok()
            .map(|entries| PackScan {
                entries,
                exclude: self
                    .sources
                    .get(exclude)
                    .map(|source| source.lookup.pack_path().to_path_buf()),
            });
        Stage::Redundant {
            packs,
            alternates: None,
            error,
        }
    }

    /// Start a lookup: the registry-selected pack copy first, so a corrupt
    /// loose file never shadows a good packed copy.
    fn start(&mut self, store: usize, oid: ObjectId) -> Result<Start> {
        let db = self.store(store);
        if let Some(object) = implied_empty_tree_object(db.format, &oid) {
            return Ok(Start::Resolved(object));
        }
        if let Some(lookup) = db.find_pack_containing(&oid)? {
            // A warm decoded-object hit needs no pack source at all.
            if let Some(object) = self.cached(store, &oid) {
                return Ok(Start::Resolved(object));
            }
            let location = self.copy(store, oid, lookup);
            let Location::Copy { source, .. } = location else {
                return Err(invariant("pack copy location"));
            };
            return Ok(Start::Attempt(location, Stage::Primary { source }));
        }
        self.without_pack_copy(store, oid)
    }

    /// Complete a packed read of `oid`: verify it if paranoid reads are on,
    /// then publish it to the decoded-object cache.
    fn finish_copy(
        &self,
        store: usize,
        oid: ObjectId,
        object: Arc<EncodedObject>,
    ) -> Result<Arc<EncodedObject>> {
        let db = self.store(store);
        // Trust the index's offset mapping unless paranoid read verification
        // is enabled; re-hashing dominated bulk reads.
        if verify_reads_enabled() {
            let actual = object.object_id(db.format)?;
            if actual != oid {
                return Err(GitError::InvalidObject(format!(
                    "pack object id mismatch: index says {oid}, decoded {actual}"
                )));
            }
        }
        if let Ok(mut cache) = db.decoded.lock() {
            cache.put(oid, Arc::clone(&object));
        }
        Ok(object)
    }

    /// No usable pack copy: loose storage, a re-selected pack, then the
    /// alternates.
    fn without_pack_copy(&mut self, store: usize, oid: ObjectId) -> Result<Start> {
        let db = self.store(store);
        let loose_error = match db.loose.read_object(&oid) {
            Ok(object) => return Ok(Start::Resolved(object)),
            Err(GitError::NotFound(_)) => None,
            Err(error) => Some(error),
        };
        if let Some(object) = self.cached(store, &oid) {
            return Ok(Start::Resolved(object));
        }
        if let Some(lookup) = self.store(store).find_pack_containing(&oid)? {
            return Ok(Start::Attempt(
                self.copy(store, oid, lookup),
                Stage::Reselected,
            ));
        }
        let mut stage = Stage::Alternates {
            remaining: self.alternate_locations(store, oid),
            loose_error,
        };
        Ok(match self.advance(store, oid, &mut stage)? {
            Next::Resolved(object) => Start::Resolved(object),
            Next::Attempt(location) => Start::Attempt(location, stage),
        })
    }

    /// The next copy to try in `stage`, or its final outcome.
    fn advance(&mut self, store: usize, oid: ObjectId, stage: &mut Stage) -> Result<Next> {
        match stage {
            Stage::Primary { .. } | Stage::Reselected => {
                Err(invariant("single-copy stage advanced"))
            }
            Stage::Alternates {
                remaining,
                loose_error,
            } => {
                if let Some(location) = remaining.pop_front() {
                    return Ok(Next::Attempt(location));
                }
                // Reprepare on miss: a sibling handle may have written the
                // object loose after the loose cache was built.
                let loose = &self.store(store).loose;
                loose.invalidate_cache();
                match loose.read_object(&oid) {
                    Ok(object) => return Ok(Next::Resolved(object)),
                    Err(GitError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
                Err(loose_error.take().unwrap_or_else(|| {
                    GitError::object_not_found_in(oid, MissingObjectContext::Read)
                }))
            }
            Stage::Redundant {
                packs,
                alternates,
                error,
            } => {
                if let Some(scan) = packs {
                    match self.next_pack_copy(store, oid, scan)? {
                        Some(location) => return Ok(Next::Attempt(location)),
                        None => *packs = None,
                    }
                }
                let alternates =
                    alternates.get_or_insert_with(|| self.alternate_locations(store, oid));
                match alternates.pop_front() {
                    Some(location) => Ok(Next::Attempt(location)),
                    None => Err(std::mem::replace(
                        error,
                        GitError::object_not_found_in(oid, MissingObjectContext::Read),
                    )),
                }
            }
        }
    }

    fn enter_packed(
        &mut self,
        source: usize,
        offset: u64,
    ) -> Result<DeltaChainStep<Location, Entry, Arc<EncodedObject>>> {
        if let Some(cache) = &self.sources[source].cache
            && let Some(object) = PackDeltaCacheAdapter(cache).get(offset)
        {
            return Ok(DeltaChainStep::Resolved(object));
        }
        if self.sources[source].bytes.is_none() {
            let state = &self.sources[source];
            let bytes = state.lookup.pack_bytes(self.store(state.store))?;
            self.sources[source].bytes = Some(bytes);
        }
        let state = &self.sources[source];
        let store = state.store;
        let Some(bytes) = &state.bytes else {
            return Err(invariant("pack source without bytes"));
        };
        let entry = read_pack_entry_at(bytes, offset, self.store(store).format)?;
        let base = match entry.base() {
            None => {
                return Ok(DeltaChainStep::Base(Entry::Packed {
                    source,
                    offset,
                    entry,
                    recovered: false,
                }));
            }
            Some(&DeltaBase::Offset(base)) => {
                // In bulk reads the base is usually decoded already: apply the
                // delta now rather than queueing a frame for a cache hit.
                if let Some(cache) = &state.cache
                    && let Some(base_object) = PackDeltaCacheAdapter(cache).get(base)
                {
                    let object = entry.resolve(Some(&base_object))?;
                    PackDeltaCacheAdapter(cache).insert(offset, Arc::clone(&object));
                    return Ok(DeltaChainStep::Resolved(object));
                }
                Location::Entry(source, base)
            }
            Some(&DeltaBase::Ref(oid)) => Location::Object(store, oid),
            Some(_) => return Err(invariant("unsupported delta base kind")),
        };
        Ok(DeltaChainStep::Pending(
            Entry::Packed {
                source,
                offset,
                entry,
                recovered: false,
            },
            DeltaChainBase::Location(base),
        ))
    }

    /// A copy of `lookup`'s object failed with `error`: pick the next source
    /// in store order, or report the lookup's final error.
    fn recover_lookup(&mut self, lookup: &mut Lookup, error: GitError) -> Result<Next> {
        let (store, oid) = (lookup.store, lookup.oid);
        match &mut lookup.stage {
            Stage::Primary { .. } if matches!(error, GitError::NotFound(_)) => {
                Ok(match self.without_pack_copy(store, oid)? {
                    Start::Resolved(object) => Next::Resolved(object),
                    Start::Attempt(location, stage) => {
                        lookup.stage = stage;
                        Next::Attempt(location)
                    }
                })
            }
            // A corrupt packed copy is not fatal while another good copy
            // exists: loose, other packs, then alternates.
            Stage::Primary { source } => {
                let source = *source;
                if let Ok(object) = self.store(store).loose.read_object(&oid) {
                    return Ok(Next::Resolved(object));
                }
                lookup.stage = self.redundant_stage(store, source, error);
                self.advance(store, oid, &mut lookup.stage)
            }
            Stage::Reselected => Err(error),
            Stage::Alternates { .. } if !matches!(error, GitError::NotFound(_)) => Err(error),
            stage => self.advance(store, oid, stage),
        }
    }

    /// Resolve one copy for the top-level lookup. A packed copy's entry chain
    /// runs on the work stack and its verification and caching happen here,
    /// so a plain packed read needs no lookup or copy frames.
    fn resolve_copy(
        &mut self,
        location: Location,
        cache_checked: bool,
    ) -> Result<Arc<EncodedObject>> {
        let Location::Copy {
            source,
            offset,
            oid,
        } = location
        else {
            return resolve_delta_chain(self, location);
        };
        let store = self.sources[source].store;
        if !cache_checked && let Some(object) = self.cached(store, &oid) {
            return Ok(object);
        }
        let object = resolve_delta_chain(self, Location::Entry(source, offset))?;
        self.finish_copy(store, oid, object)
    }

    /// An OFS base failed to decode in its own pack: find the base object's
    /// id from the index and read any other copy of it, or report the
    /// original error when there is none.
    fn recover_ofs_base(
        &mut self,
        source: usize,
        base_offset: u64,
        error: GitError,
    ) -> Result<DeltaChainBase<Location, Arc<EncodedObject>>> {
        let store = self.sources[source].store;
        let db = self.store(store);
        let Some(oid) = db.pack_oid_at_offset(&self.sources[source].lookup, base_offset)? else {
            return Err(error);
        };
        if let Some(object) = self.cached(store, &oid) {
            return Ok(DeltaChainBase::Resolved(object));
        }
        if let Ok(object) = self.store(store).loose.read_object(&oid) {
            return Ok(DeltaChainBase::Resolved(object));
        }
        let lookup = Lookup {
            store,
            oid,
            stage: self.redundant_stage(store, source, error),
        };
        self.queued.push(Some(lookup));
        Ok(DeltaChainBase::Location(Location::Queued(
            self.queued.len() - 1,
        )))
    }
}

/// An internal sequencing error; reported rather than panicking.
fn invariant(what: &str) -> GitError {
    GitError::InvalidFormat(format!("object read state machine: {what}"))
}

fn lookup_step(lookup: Lookup, next: Next) -> DeltaChainStep<Location, Entry, Arc<EncodedObject>> {
    match next {
        Next::Resolved(object) => DeltaChainStep::Resolved(object),
        Next::Attempt(location) => {
            DeltaChainStep::Pending(Entry::Lookup(lookup), DeltaChainBase::Location(location))
        }
    }
}

fn next_base(next: Next) -> DeltaChainBase<Location, Arc<EncodedObject>> {
    match next {
        Next::Resolved(object) => DeltaChainBase::Resolved(object),
        Next::Attempt(location) => DeltaChainBase::Location(location),
    }
}

impl DeltaChainResolver for Reader<'_> {
    type Location = Location;
    type Entry = Entry;
    type Object = Arc<EncodedObject>;
    type Error = GitError;

    fn enter(
        &mut self,
        location: Location,
        _pending: usize,
    ) -> Result<DeltaChainStep<Location, Entry, Arc<EncodedObject>>> {
        match location {
            Location::Object(store, oid) => Ok(match self.start(store, oid)? {
                Start::Resolved(object) => DeltaChainStep::Resolved(object),
                Start::Attempt(location, stage) => DeltaChainStep::Pending(
                    Entry::Lookup(Lookup { store, oid, stage }),
                    DeltaChainBase::Location(location),
                ),
            }),
            Location::Queued(id) => {
                let mut lookup = self
                    .queued
                    .get_mut(id)
                    .and_then(Option::take)
                    .ok_or_else(|| invariant("queued lookup entered twice"))?;
                let next = self.advance(lookup.store, lookup.oid, &mut lookup.stage)?;
                Ok(lookup_step(lookup, next))
            }
            Location::Copy {
                source,
                offset,
                oid,
            } => {
                let store = self.sources[source].store;
                // Same order as a packed object read: the decoded-object
                // cache, then this copy's entry.
                if let Some(object) = self.cached(store, &oid) {
                    return Ok(DeltaChainStep::Resolved(object));
                }
                Ok(DeltaChainStep::Pending(
                    Entry::Copy { store, oid },
                    DeltaChainBase::Location(Location::Entry(source, offset)),
                ))
            }
            Location::Entry(source, offset) => self.enter_packed(source, offset),
        }
    }

    fn finish(
        &mut self,
        entry: Entry,
        base: Option<Arc<EncodedObject>>,
    ) -> Result<Arc<EncodedObject>> {
        match entry {
            Entry::Packed {
                source,
                offset,
                entry,
                ..
            } => {
                let object = entry.resolve(base.as_deref())?;
                if let Some(cache) = &self.sources[source].cache {
                    PackDeltaCacheAdapter(cache).insert(offset, Arc::clone(&object));
                }
                Ok(object)
            }
            Entry::Copy { store, oid } => {
                let object = base.ok_or_else(|| invariant("packed copy without an entry"))?;
                self.finish_copy(store, oid, object)
            }
            Entry::Lookup(_) => base.ok_or_else(|| invariant("object lookup without a copy")),
        }
    }

    fn recover(
        &mut self,
        entry: &mut Entry,
        error: GitError,
    ) -> Result<DeltaChainBase<Location, Arc<EncodedObject>>> {
        match entry {
            Entry::Lookup(lookup) => self.recover_lookup(lookup, error).map(next_base),
            Entry::Packed {
                source,
                entry,
                recovered,
                ..
            } => match entry.base() {
                Some(&DeltaBase::Offset(base_offset)) if !*recovered => {
                    *recovered = true;
                    self.recover_ofs_base(*source, base_offset, error)
                }
                _ => Err(error),
            },
            Entry::Copy { .. } => Err(error),
        }
    }

    /// Object lookups and packed copies can be reached again: a REF base
    /// re-enters a lookup, and recovering a corrupt OFS base re-enters a copy
    /// of it in another pack, whose own recovery can lead back. Both come
    /// from finite sets, so any endless descent repeats one of them while it
    /// is still pending. Pack entries lead only to strictly earlier OFS
    /// entries, and queued lookups are entered once.
    fn tracks_cycles(&self, location: &Location) -> bool {
        matches!(location, Location::Object(..) | Location::Copy { .. })
    }

    fn cycle_error(&self) -> GitError {
        GitError::InvalidFormat("pack delta cycle detected".into())
    }
}
