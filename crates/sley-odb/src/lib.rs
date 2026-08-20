// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use sley_core::{GitError, MissingObjectContext, ObjectFormat, ObjectId, Result};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::PackIndexEntry;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

mod gc;
mod install;
mod loose;
mod midx;
mod pack;
mod reachability;
mod registry;
mod repack;

pub use gc::*;
pub use install::*;
pub use loose::*;
pub use midx::*;
// Re-exported so callers of the progress-aware installer can name the counter
// struct without depending on `sley-pack` directly.
pub use pack::*;
pub use reachability::*;
pub use registry::*;
pub use repack::*;
pub use sley_pack::PackStreamProgress;
// Cancel types re-exported so callers of cancel-aware install (remote, fetch)
// can name them without depending on `sley-pack` or reaching into `sley-core`.
pub use sley_core::{AtomicCancel, CancelFlag, CancellableRead};

static TEMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One existing pack offered for transfer-pack entry reuse.
///
/// As with [`ObjectReader::read_object`], the reader guarantees that each oid
/// maps to its canonical object. Reachable-pack generation independently
/// verifies the pack checksum, header, entry boundaries, and every reused
/// delta base before emitting any candidate bytes.
#[derive(Debug, Clone)]
pub struct ReusablePackCandidate {
    /// In-memory pack bytes supplied by generic readers.
    pub pack: Arc<[u8]>,
    /// File-backed pack source supplied by filesystem readers. When present,
    /// reachable-pack reuse validates and streams this path through the ODB's
    /// mmap-capable loader instead of copying the complete pack into the heap.
    pub pack_path: Option<PathBuf>,
    pub entries: Vec<PackIndexEntry>,
    pub pack_checksum: ObjectId,
}

pub trait ObjectReader {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>>;

    /// Return existing packs that may contain entries for `object_ids`.
    ///
    /// Pack-backed readers can override this to let transfer-pack generation
    /// copy compressed entries and existing delta instructions instead of
    /// recomputing them. The builder validates candidates and falls back to
    /// ordinary generation for every unsafe entry. Readers without pack
    /// provenance keep the default empty result.
    fn reusable_pack_candidates(
        &self,
        _object_ids: &HashSet<ObjectId>,
    ) -> Result<Vec<ReusablePackCandidate>> {
        Ok(Vec::new())
    }

    /// Return the immediate on-disk delta base for `oid`, when the reader can
    /// expose storage metadata without decoding the object body.
    ///
    /// Transfer-pack generation uses this optional hint to preserve an
    /// existing delta whose base is already owned by the receiver. Readers
    /// without pack storage keep the default `None` behavior.
    fn reusable_delta_base(&self, _oid: &ObjectId) -> Result<Option<ObjectId>> {
        Ok(None)
    }

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
pub trait ObjectWriter {
    /// Write `object`, returning its id. Takes `&self`: every implementation's
    /// write state (in-memory map, loose-object cache) is behind interior
    /// mutability, so a single handle can interleave reads and writes without a
    /// `&mut` borrow. This lets the merge engine read and write through one `db`
    /// instead of opening a second read-only handle that re-warms the caches.
    fn write_object(&self, object: EncodedObject) -> Result<ObjectId>;
}

pub(crate) fn implied_empty_tree_object(
    format: ObjectFormat,
    oid: &ObjectId,
) -> Option<Arc<EncodedObject>> {
    (*oid == ObjectId::empty_tree(format))
        .then(|| Arc::new(EncodedObject::new(ObjectType::Tree, Vec::new())))
}

pub(crate) fn with_missing_object_context(
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
pub(crate) fn unique_temp_path(parent: &Path) -> PathBuf {
    let id = TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("tmp_obj_{}_{}", std::process::id(), id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use sley_core::{BString, GitError};
    use sley_formats::Bundle;
    use sley_object::{Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntry};
    use sley_pack::{MultiPackIndex, PackBitmapIndex, PackFile, PackIndex, PackWriteOptions};
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::io::{Read, Write};

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
    fn injected_replacements_affect_reads_but_not_raw_enumeration() {
        let root = temp_root("replacement_reads");
        let db = FileObjectDatabase::new(root.join("objects"), ObjectFormat::Sha1);
        let original = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"original".to_vec()))
            .expect("write original");
        let replacement = db
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"replacement".to_vec(),
            ))
            .expect("write replacement");
        let replacing = db
            .clone()
            .with_replacements(ObjectReplacements::new([(original, replacement)]));

        assert_eq!(
            replacing
                .read_object(&original)
                .expect("replacement read")
                .body,
            b"replacement"
        );
        assert_eq!(
            replacing
                .read_object_header(&original)
                .expect("replacement header"),
            Some((ObjectType::Blob, 11))
        );
        let ids = replacing.object_ids().expect("raw object enumeration");
        assert!(ids.contains(&original));
        assert!(ids.contains(&replacement));
        assert_eq!(
            db.read_object(&original).expect("raw read").body,
            b"original"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replacement_policy_rejects_cycles() {
        let first = sley_core::digest_bytes(ObjectFormat::Sha1, b"first").expect("first oid");
        let second = sley_core::digest_bytes(ObjectFormat::Sha1, b"second").expect("second oid");
        let replacements = ObjectReplacements::new([(first, second), (second, first)]);
        assert!(matches!(
            replacements.resolve(&first),
            Err(GitError::InvalidObject(message)) if message.contains("replace depth too high")
        ));
    }

    #[test]
    fn replacement_policy_treats_identity_mapping_as_noop() {
        let oid = sley_core::digest_bytes(ObjectFormat::Sha1, b"identity").expect("identity oid");
        let replacements = ObjectReplacements::new([(oid, oid)]);
        assert_eq!(replacements.resolve(&oid).expect("identity mapping"), oid);
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
        // Header reads prefer pack (sley#26), so a packed-only object never
        // consults loose storage — and must not create or scan a fanout dir.
        assert_eq!(
            db.read_object_header(&oid)
                .expect("test operation should succeed"),
            Some((ObjectType::Blob, object.body.len() as u64))
        );
        assert_eq!(read_object_for_assert(&db, &oid), object);

        let fanout_hex = format!("{:02x}", oid.as_bytes()[0]);
        assert!(
            !git_dir.join("objects").join(&fanout_hex).exists(),
            "reading a packed object must not create its loose fanout dir"
        );
        if let Ok(guard) = db.loose().loose_cache.lock() {
            assert_eq!(
                guard.present_fanouts.as_ref(),
                None,
                "a packed-only header/body read must not probe loose fanouts"
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
    fn loose_ofs_delta_base_recovers_child_from_corrupt_pack_base() {
        let root = temp_root("sley-ofs-delta-base-corrupt-loose-recovery");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let mut base_body = Vec::new();
        for idx in 0..256u16 {
            base_body.extend_from_slice(format!("shared line {idx:03}\n").as_bytes());
        }
        let base = EncodedObject::new(ObjectType::Blob, base_body.clone());
        let mut first_body = base_body;
        first_body.extend_from_slice(b"first delta payload\n");
        let first = EncodedObject::new(ObjectType::Blob, first_body.clone());
        let mut second_body = first_body;
        second_body.extend_from_slice(b"second delta payload\n");
        let second = EncodedObject::new(ObjectType::Blob, second_body);
        let base_oid = base
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let first_oid = first
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let second_oid = second
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        let options = PackWriteOptions::new()
            .with_prefer_ofs_delta(true)
            .with_reorder(false);
        let written = PackFile::write_packed_with_options(
            &[base, first.clone(), second.clone()],
            ObjectFormat::Sha1,
            &options,
        )
        .expect("test operation should succeed");
        let stats = PackFile::verify_pack_stats(&written.pack, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let first_stat = stats
            .objects
            .iter()
            .find(|stat| stat.oid == first_oid)
            .expect("first object should be packed");
        let second_stat = stats
            .objects
            .iter()
            .find(|stat| stat.oid == second_oid)
            .expect("second object should be packed");
        assert_eq!(first_stat.base_oid, Some(base_oid));
        assert_eq!(second_stat.base_oid, Some(first_oid));

        let installed = db
            .install_pack(&written)
            .expect("test operation should succeed");
        db.loose()
            .write_object(first.clone())
            .expect("test operation should succeed");

        let mut corrupt_pack = written.pack;
        let base_reference = ofs_delta_base_reference_position(&corrupt_pack, first_stat.offset);
        corrupt_pack[base_reference] = if corrupt_pack[base_reference] == 1 {
            2
        } else {
            1
        };
        assert!(PackFile::verify_pack_stats(&corrupt_pack, ObjectFormat::Sha1).is_err());
        fs::write(&installed.pack_path, &corrupt_pack).expect("test operation should succeed");
        db.refresh_read_cache();

        assert_eq!(read_object_for_assert(&db, &first_oid), first);
        assert_eq!(read_object_for_assert(&db, &second_oid), second);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn ofs_delta_base_reference_position(pack: &[u8], offset: u64) -> usize {
        let mut cursor = usize::try_from(offset).expect("test operation should succeed");
        let first = pack[cursor];
        cursor += 1;
        let kind = (first >> 4) & 0x07;
        let mut byte = first;
        while byte & 0x80 != 0 {
            byte = pack[cursor];
            cursor += 1;
        }
        assert_eq!(kind, 6, "expected an ofs-delta entry");
        cursor
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

    /// Write a one-entry pack whose index claims `indexed_oid` for a ref-delta
    /// based on `base_oid`. This intentionally bypasses pack installation's
    /// integrity validation so header reads can be tested against corrupt
    /// on-disk metadata that may be produced by disk damage.
    fn write_indexed_ref_delta(
        pack_dir: &Path,
        format: ObjectFormat,
        indexed_oid: ObjectId,
        base_oid: ObjectId,
    ) {
        fs::create_dir_all(pack_dir).expect("test operation should succeed");
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());

        let offset = pack.len() as u64;
        let delta_header = [0u8, 0u8]; // base size 0, result size 0
        pack.push(0x70 | delta_header.len() as u8); // ref-delta, size 2
        pack.extend_from_slice(base_oid.as_bytes());
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&delta_header)
            .expect("test operation should succeed");
        pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));

        let crc32 = crc32fast::hash(&pack[offset as usize..]);
        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        let index = PackIndex::write_v2(
            format,
            &[PackIndexEntry {
                oid: indexed_oid,
                crc32,
                offset,
            }],
            &checksum,
        )
        .expect("test operation should succeed");
        let name = checksum.to_hex();
        fs::write(pack_dir.join(format!("pack-{name}.pack")), pack)
            .expect("test operation should succeed");
        fs::write(pack_dir.join(format!("pack-{name}.idx")), index)
            .expect("test operation should succeed");
    }

    #[test]
    fn read_object_header_ref_delta_cycle_falls_back_to_loose_copy() {
        let root = temp_root("sley-header-ref-delta-cycle");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let object = EncodedObject::new(ObjectType::Blob, b"good loose fallback\n".to_vec());
        let oid = db
            .write_object(object.clone())
            .expect("test operation should succeed");
        let other_oid = sley_core::object_id_for_bytes(format, "blob", b"cycle peer")
            .expect("test operation should succeed");

        // The corrupt indexes form `oid -> other_oid -> oid`. Header resolution
        // must reject the cycle before recursively consulting stores, then use
        // the complete loose copy of the requested object.
        write_indexed_ref_delta(&db.pack_dir, format, oid, other_oid);
        write_indexed_ref_delta(&db.pack_dir, format, other_oid, oid);
        db.refresh_read_cache();
        assert_eq!(
            db.read_object_header(&oid)
                .expect("the loose fallback should satisfy the header read"),
            Some((ObjectType::Blob, object.body.len() as u64))
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn read_object_header_preserves_missing_ref_delta_base_error() {
        let root = temp_root("sley-header-missing-ref-delta-base");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        let format = ObjectFormat::Sha1;
        let oid = sley_core::object_id_for_bytes(format, "blob", b"indexed object")
            .expect("test operation should succeed");
        let missing_base = sley_core::object_id_for_bytes(format, "blob", b"missing base")
            .expect("test operation should succeed");
        write_indexed_ref_delta(&pack_dir, format, oid, missing_base);

        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let error = db
            .read_object_header(&oid)
            .expect_err("an indexed object with no delta base must be corrupt, not absent");
        assert!(
            error
                .to_string()
                .contains(&format!("ref-delta base object {missing_base}")),
            "the packed-header error must retain the missing base: {error}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn read_object_header_falls_back_from_corrupt_midx_and_preserves_lookup_error() {
        let root = temp_root("sley-header-corrupt-midx-fallback");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let object = EncodedObject::new(ObjectType::Blob, b"loose despite corrupt midx\n".to_vec());
        let oid = db
            .write_object(object.clone())
            .expect("test operation should succeed");
        fs::create_dir_all(&db.pack_dir).expect("test operation should succeed");
        fs::write(&db.midx_path, b"MIDX").expect("test operation should succeed");

        assert_eq!(
            db.read_object_header(&oid)
                .expect("the valid loose copy should survive corrupt lookup metadata"),
            Some((ObjectType::Blob, object.body.len() as u64))
        );

        let missing = sley_core::object_id_for_bytes(format, "blob", b"actually missing")
            .expect("test operation should succeed");
        let error = db
            .read_object_header(&missing)
            .expect_err("lookup corruption must not be flattened into a missing object");
        assert!(
            error.to_string().contains("multi-pack-index"),
            "the original pack lookup error must be retained: {error}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn read_object_header_prefers_good_pack_over_corrupt_loose() {
        // sley#26: the header path used to probe loose first, so a corrupt loose
        // copy aborted --batch-check even when the pack was fine. Pack-first
        // matches `read_object` and never opens the loose file.
        let root = temp_root("sley-header-pack-over-corrupt-loose");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let object = EncodedObject::new(ObjectType::Blob, vec![b'h'; 2048]);
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        db.install_pack(&pack)
            .expect("test operation should succeed");
        db.loose()
            .write_object(object.clone())
            .expect("test operation should succeed");
        let loose_path = db
            .loose()
            .object_path(&oid)
            .expect("test operation should succeed");
        fs::write(&loose_path, b"this is not a zlib stream")
            .expect("test operation should succeed");
        db.refresh_read_cache();

        assert_eq!(
            db.read_object_header(&oid)
                .expect("test operation should succeed"),
            Some((ObjectType::Blob, object.body.len() as u64)),
            "a corrupt loose copy must not shadow a good packed header"
        );
        assert_eq!(read_object_for_assert(&db, &oid), object);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn read_object_header_ofs_delta_batch_matches_full_read_cold_and_warm() {
        // Two passes over the same ofs-delta pack: the first populates the
        // per-pack header memo, the second must stay byte-identical (sley#26).
        let root = temp_root("sley-header-ofs-delta-batch");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let objects = (0..8)
            .map(|index| {
                let mut body = vec![b'a'; 1024];
                body.extend_from_slice(format!(" tail {index}\n").as_bytes());
                EncodedObject::new(ObjectType::Blob, body)
            })
            .collect::<Vec<_>>();
        let oids = objects
            .iter()
            .map(|object| {
                object
                    .object_id(format)
                    .expect("test operation should succeed")
            })
            .collect::<Vec<_>>();
        let options = PackWriteOptions::new()
            .with_prefer_ofs_delta(true)
            .with_reorder(false);
        let pack = PackFile::write_packed_with_options(&objects, format, &options)
            .expect("test operation should succeed");
        db.install_pack(&pack)
            .expect("test operation should succeed");

        let mut first_pass = Vec::with_capacity(oids.len());
        for (oid, object) in oids.iter().zip(&objects) {
            let header = db
                .read_object_header(oid)
                .expect("test operation should succeed");
            assert_eq!(
                header,
                Some((ObjectType::Blob, object.body.len() as u64)),
                "cold header for {oid}"
            );
            first_pass.push(header);
        }
        for (oid, want) in oids.iter().zip(&first_pass) {
            assert_eq!(
                db.read_object_header(oid)
                    .expect("test operation should succeed"),
                *want,
                "warm header for {oid}"
            );
            let full = db.read_object(oid).expect("test operation should succeed");
            assert_eq!(
                *want,
                Some((full.object_type, full.body.len() as u64)),
                "header must match a full decode for {oid}"
            );
        }
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
    fn file_database_completes_thin_pack_before_installing_it() {
        let root = temp_root("sley-file-odb-install-fix-thin");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object database");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let base = EncodedObject::new(ObjectType::Blob, vec![b'a'; 4096]);
        let base_oid = db.write_object(base.clone()).expect("write external base");
        let mut target = base.clone();
        target.body[2048] = b'b';
        let target_oid = target.object_id(format).expect("target oid");
        let thin = PackFile::write_thin(
            std::slice::from_ref(&target),
            format,
            HashMap::from([(base_oid, base)]),
        )
        .expect("write thin pack");
        assert!(PackFile::parse(&thin.pack, format).is_err());

        let installed = db
            .install_raw_pack_from_reader_with_external_bases(&mut thin.pack.as_slice())
            .expect("install completed pack");
        let installed_bytes = fs::read(&installed.pack_path).expect("read installed pack");
        let parsed = PackFile::parse(&installed_bytes, format)
            .expect("installed pack must resolve without the database");
        assert_eq!(parsed.entries.len(), 2);
        assert!(installed.object_ids.contains(&base_oid));
        assert!(installed.object_ids.contains(&target_oid));

        fs::remove_file(
            db.loose()
                .object_path(&base_oid)
                .expect("external base path"),
        )
        .expect("remove loose external base");
        let reopened = FileObjectDatabase::from_git_dir(&git_dir, format);
        assert_eq!(
            reopened
                .read_object(&target_oid)
                .expect("read target from standalone pack")
                .as_ref(),
            &target
        );
        fs::remove_dir_all(root).expect("remove thin install fixture");
    }

    #[test]
    fn incoming_pack_quarantine_discards_rejected_and_promotes_accepted_objects() {
        let root = temp_root("sley-incoming-pack-quarantine");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects/pack")).expect("create object database");
        let format = ObjectFormat::Sha1;
        let destination = FileObjectDatabase::from_git_dir(&git_dir, format);
        let existing = destination
            .write_object(EncodedObject::new(ObjectType::Blob, b"existing\n".to_vec()))
            .expect("write existing object");
        let borrowed_git_dir = root.join("borrowed.git");
        fs::create_dir_all(borrowed_git_dir.join("objects/pack"))
            .expect("create borrowed object database");
        let borrowed_db = FileObjectDatabase::from_git_dir(&borrowed_git_dir, format);
        let borrowed = borrowed_db
            .write_object(EncodedObject::new(ObjectType::Blob, b"borrowed\n".to_vec()))
            .expect("write borrowed object");
        fs::create_dir_all(git_dir.join("objects/info")).expect("create alternate metadata");
        fs::write(
            git_dir.join("objects/info/alternates"),
            format!("{}\n", borrowed_git_dir.join("objects").display()),
        )
        .expect("write destination alternate");
        let incoming = EncodedObject::new(ObjectType::Blob, b"incoming\n".to_vec());
        let incoming_oid = incoming.object_id(format).expect("incoming oid");
        let pack = PackFile::write_packed(&[&incoming], format).expect("write incoming pack");

        {
            let quarantine =
                IncomingPackQuarantine::new(&git_dir, format).expect("create rejected quarantine");
            let db = quarantine.database();
            db.install_pack(&pack).expect("stage rejected pack");
            assert!(db.contains(&existing).expect("read alternate object"));
            assert!(
                db.contains(&borrowed)
                    .expect("read destination's borrowed object")
            );
            assert!(db.contains(&incoming_oid).expect("read staged object"));
        }
        let reopened = FileObjectDatabase::from_git_dir(&git_dir, format);
        assert!(
            reopened
                .contains(&existing)
                .expect("existing object remains")
        );
        assert!(
            !reopened
                .contains(&incoming_oid)
                .expect("rejected object absent")
        );

        let quarantine =
            IncomingPackQuarantine::new(&git_dir, format).expect("create accepted quarantine");
        quarantine
            .database()
            .install_pack(&pack)
            .expect("stage accepted pack");
        quarantine.promote().expect("promote accepted pack");
        let reopened = FileObjectDatabase::from_git_dir(&git_dir, format);
        assert!(
            reopened
                .contains(&incoming_oid)
                .expect("accepted object persists")
        );
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
    fn file_database_rejects_raw_pack_stream_exceeding_max_input_size() {
        let root = temp_root("sley-file-odb-install-raw-pack-max-size");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"bounded raw pack install\n".to_vec());
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let pack_dir = git_dir.join("objects").join("pack");
        let before = fs::read_dir(&pack_dir)
            .map(|entries| entries.count())
            .unwrap_or_default();
        let mut reader = pack.pack.as_slice();
        let limit = 32u64;

        let err = db
            .install_raw_pack_from_reader_with_options(
                &mut reader,
                RawPackInstallOptions {
                    max_input_size: Some(limit),
                    ..Default::default()
                },
            )
            .expect_err("oversized stream should be rejected");

        assert!(
            err.to_string()
                .contains("pack exceeds maximum allowed size"),
            "unexpected error: {err}"
        );
        let temp_files = fs::read_dir(&pack_dir)
            .expect("pack dir should exist")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("tmp_obj_"))
            })
            .collect::<Vec<_>>();
        assert!(
            temp_files.is_empty(),
            "temp pack staging file should be removed on failure"
        );
        let after = fs::read_dir(&pack_dir)
            .map(|entries| entries.count())
            .unwrap_or_default();
        assert_eq!(after, before, "no durable pack files should be installed");
        let installed = fs::read_dir(&pack_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack")
                    })
                    .count()
            })
            .unwrap_or_default();
        assert_eq!(installed, 0);
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
    fn file_database_cancels_raw_pack_install_and_cleans_temp() {
        let root = temp_root("sley-file-odb-install-raw-pack-cancel");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let object = EncodedObject::new(ObjectType::Blob, b"cancelled raw pack install\n".to_vec());
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let pack_dir = git_dir.join("objects").join("pack");
        let source = AtomicCancel::new();
        source.cancel();
        let cancel = CancelFlag::new(&source);
        let mut reader = pack.pack.as_slice();

        let err = db
            .install_raw_pack_from_reader_with_progress_and_cancel(
                &mut reader,
                RawPackInstallOptions::default(),
                cancel,
                |_| {},
            )
            .expect_err("pre-set cancel should abort install");

        assert!(
            matches!(err, GitError::Cancelled),
            "expected Cancelled, got {err:?}"
        );
        // Early cancel.check() should avoid creating a temp file; even if a
        // staging file was opened, error cleanup must leave pack/ empty of
        // tmp_obj_* and durable pack/idx files.
        if pack_dir.exists() {
            let leftovers = fs::read_dir(&pack_dir)
                .expect("pack dir should be readable")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .collect::<Vec<_>>();
            assert!(
                leftovers.is_empty(),
                "no leftover pack staging files after cancel: {leftovers:?}"
            );
        }
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
                RawPackInstallOptions {
                    promisor: true,
                    ..Default::default()
                },
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
    fn file_database_seeds_promisor_objects_from_alternate_packs() {
        let root = temp_root("sley-file-odb-alternate-promisor-pack");
        let local_objects = root.join("local-objects");
        let alternate_objects = root.join("alternate-objects");
        fs::create_dir_all(local_objects.join("info")).expect("create local object directory");
        fs::create_dir_all(&alternate_objects).expect("create alternate object directory");
        fs::write(
            local_objects.join("info").join("alternates"),
            format!("{}\n", alternate_objects.display()),
        )
        .expect("write alternates file");

        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"alternate promisor object\n".to_vec());
        let oid = object.object_id(format).expect("object id");
        let pack =
            PackFile::write_undeltified(std::slice::from_ref(&object), format).expect("write pack");
        FileObjectDatabase::new(&alternate_objects, format)
            .install_pack_with_options(
                &pack,
                RawPackInstallOptions {
                    promisor: true,
                    ..Default::default()
                },
            )
            .expect("install alternate promisor pack");

        let db = FileObjectDatabase::new(&local_objects, format).with_promisor_remote_present(true);
        assert!(
            db.is_promised_object(&oid),
            "alternate .promisor packs must seed the promised-object boundary"
        );

        fs::remove_dir_all(root).expect("remove test repository");
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
            .write_object(object)
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
        let shared = "streamed reachable shared payload ".repeat(16);
        for idx in 0..(REACHABLE_PACK_STREAMING_MIN_OBJECTS + 5) {
            let object = EncodedObject::new(
                ObjectType::Blob,
                // Large enough that the tiny varying suffix remains inside
                // Git's half-target-minus-object-id delta budget.
                format!("{shared}{idx:04}\n").into_bytes(),
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
            fn install_raw_pack_from_reader_with_options<R>(
                &self,
                reader: &mut R,
                _options: RawPackInstallOptions,
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
    fn object_presence_checker_can_hold_a_closed_world_snapshot() {
        let root = temp_root("sley-presence-checker-closed-world");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"added after snapshot\n".to_vec());
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut checker = db.presence_checker();
        assert!(
            !checker
                .contains_without_refresh(&oid)
                .expect("test operation should succeed")
        );

        // A separate handle simulates an out-of-band writer whose loose-cache
        // update is not shared with the checker. Closed-world mode deliberately
        // holds the miss until the caller requests normal refresh semantics.
        FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1)
            .write_object(object)
            .expect("test operation should succeed");
        assert!(
            !checker
                .contains_without_refresh(&oid)
                .expect("test operation should succeed")
        );
        assert!(
            checker
                .contains(&oid)
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
        assert!(first_registry.packs[0].index.read().is_none());
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
        assert!(first_registry.packs[0].index.read().is_some());
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
        expected.insert(packed_oid, packed_blob);

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
        assert_eq!(result.obsolete_packs, vec![existing.pack_path]);
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

    fn repack_a_unpacks_unreachable_before_pruning(format: ObjectFormat) {
        let root = temp_root("sley-repack-unpack-unreachable");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        let mut database = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut database, b"reachable graph\n");
        let root_oid = graph[0].0;
        let unreachable = EncodedObject::new(ObjectType::Blob, b"packed unreachable\n".to_vec());
        let unreachable_oid = database
            .write_object(unreachable.clone())
            .expect("write unreachable");

        let mut source_objects = graph
            .iter()
            .map(|(_, object)| object.clone())
            .collect::<Vec<_>>();
        source_objects.push(unreachable);
        let source =
            PackFile::write_undeltified(&source_objects, format).expect("write source pack");
        let installed = database.install_pack(&source).expect("install source pack");
        for oid in graph
            .iter()
            .map(|(oid, _)| *oid)
            .chain(std::iter::once(unreachable_oid))
        {
            fs::remove_file(database.loose().object_path(&oid).expect("loose path"))
                .expect("remove loose source");
        }
        fs::OpenOptions::new()
            .read(true)
            .open(&installed.pack_path)
            .expect("open source pack")
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(123))
            .expect("set pack mtime");

        let outcome = repack_reachable_objects_unpack_unreachable(
            &git_dir,
            format,
            &[root_oid],
            &RepackOptions::default(),
            None,
            &[],
        )
        .expect("build -A repack");
        assert_eq!(
            outcome.unpacked_oids().collect::<Vec<_>>(),
            vec![unreachable_oid]
        );
        install_repack_with_unpacked_unreachable(&git_dir, format, &outcome, true)
            .expect("install -A repack");

        assert!(!installed.pack_path.exists());
        let loose_path = database
            .loose()
            .object_path(&unreachable_oid)
            .expect("unreachable loose path");
        assert!(loose_path.is_file());
        let mtime = fs::metadata(&loose_path)
            .expect("loose metadata")
            .modified()
            .expect("loose mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("mtime after epoch")
            .as_secs();
        assert_eq!(mtime, 123);
        let reopened = FileObjectDatabase::from_git_dir(&git_dir, format);
        assert!(reopened.read_object(&root_oid).is_ok());
        assert!(reopened.read_object(&unreachable_oid).is_ok());

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn sha1_repack_a_unpacks_unreachable_before_pruning() {
        repack_a_unpacks_unreachable_before_pruning(ObjectFormat::Sha1);
    }

    #[test]
    fn sha256_repack_a_unpacks_unreachable_before_pruning() {
        repack_a_unpacks_unreachable_before_pruning(ObjectFormat::Sha256);
    }

    fn rewriting_cruft_only_object_creates_loose_copy(format: ObjectFormat) {
        let root = temp_root("sley-cruft-freshen-loose");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        let object = EncodedObject::new(ObjectType::Blob, b"cruft-only object\n".to_vec());
        let database = FileObjectDatabase::from_git_dir(&git_dir, format);
        let oid = database.write_object(object.clone()).expect("write object");
        let loose_path = database.loose().object_path(&oid).expect("loose path");
        fs::OpenOptions::new()
            .read(true)
            .open(&loose_path)
            .expect("open loose object")
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100))
            .expect("set original object mtime");

        let result = repack_cruft(&git_dir, format, &[], None).expect("build cruft repack");
        assert_eq!(
            result.cruft.as_ref().map(|cruft| cruft.oids.as_slice()),
            Some(std::slice::from_ref(&oid))
        );
        install_cruft_repack_result(&git_dir, format, &result, true).expect("install cruft repack");

        assert!(!loose_path.exists());

        // Cruft packs preserve an mtime per object. Rewriting an object which
        // exists only in such a pack must therefore create a loose copy rather
        // than freshening the pack (and implicitly every object in it).
        let reopened = FileObjectDatabase::from_git_dir(&git_dir, format);
        assert_eq!(reopened.write_object(object).expect("rewrite object"), oid);
        assert!(loose_path.exists());
        fs::OpenOptions::new()
            .read(true)
            .open(&loose_path)
            .expect("open rewritten loose object")
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(200))
            .expect("set rewritten object mtime");

        // Repacking the same object produces the same pack checksum, but must
        // replace the mutable `.mtimes` sidecar with the fresher loose mtime.
        let refreshed = repack_cruft(&git_dir, format, &[], None).expect("refresh cruft repack");
        assert_eq!(
            refreshed.cruft.as_ref().map(|cruft| cruft.checksum),
            result.cruft.as_ref().map(|cruft| cruft.checksum)
        );
        install_cruft_repack_result(&git_dir, format, &refreshed, true)
            .expect("install refreshed cruft repack");
        let checksum = refreshed.cruft.as_ref().expect("cruft pack").checksum;
        let mtimes_path = git_dir
            .join("objects/pack")
            .join(format!("pack-{}.mtimes", checksum.to_hex()));
        let mtimes =
            sley_pack::PackMtimes::parse(&fs::read(mtimes_path).expect("read mtimes"), format, 1)
                .expect("parse mtimes");
        assert_eq!(mtimes.mtimes, vec![200]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn rewriting_sha1_cruft_only_object_creates_loose_copy() {
        rewriting_cruft_only_object_creates_loose_copy(ObjectFormat::Sha1);
    }

    #[test]
    fn rewriting_sha256_cruft_only_object_creates_loose_copy() {
        rewriting_cruft_only_object_creates_loose_copy(ObjectFormat::Sha256);
    }

    fn expired_cruft_pack_survives_source_pruning(format: ObjectFormat) {
        let root = temp_root("sley-cruft-expire-to");
        let git_dir = root.join("source.git");
        fs::create_dir_all(git_dir.join("objects")).expect("create source object directory");
        let database = FileObjectDatabase::from_git_dir(&git_dir, format);

        let stale = EncodedObject::new(ObjectType::Blob, b"stale cruft\n".to_vec());
        let stale_oid = database.write_object(stale).expect("write stale object");
        let recent = EncodedObject::new(ObjectType::Blob, b"recent cruft\n".to_vec());
        let recent_oid = database.write_object(recent).expect("write recent object");
        for (oid, mtime) in [(stale_oid, 100), (recent_oid, 200)] {
            let path = database.loose().object_path(&oid).expect("loose path");
            fs::OpenOptions::new()
                .read(true)
                .open(path)
                .expect("open loose object")
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime))
                .expect("set object mtime");
        }

        // Expiration operates on packed cruft in the real `--expire-to`
        // workflow. Establish that state first so pruning removes the stale
        // object's only local copy.
        let initial = repack_cruft(&git_dir, format, &[], None).expect("build initial cruft pack");
        install_cruft_repack_result(&git_dir, format, &initial, true)
            .expect("install initial cruft pack");

        let result = repack_cruft(&git_dir, format, &[], Some(150)).expect("build expiring repack");
        assert_eq!(
            result.cruft.as_ref().map(|pack| pack.oids.as_slice()),
            Some(std::slice::from_ref(&recent_oid))
        );
        assert_eq!(
            result.expired.as_ref().map(|pack| pack.oids.as_slice()),
            Some(std::slice::from_ref(&stale_oid))
        );

        // Mirror the CLI's safe order: prune the source first, then prove the
        // self-contained expired output can still be installed elsewhere.
        install_cruft_repack_result(&git_dir, format, &result, true)
            .expect("install source repack");
        let source = FileObjectDatabase::from_git_dir(&git_dir, format);
        assert!(source.read_object(&recent_oid).is_ok());
        assert!(source.read_object(&stale_oid).is_err());

        let destination = root.join("expired.git/objects/pack/pack");
        install_cruft_pack_at_prefix(
            format,
            result.expired.as_ref().expect("expired pack"),
            &destination,
        )
        .expect("install expired pack");
        let expired = FileObjectDatabase::new(root.join("expired.git/objects"), format);
        assert!(expired.read_object(&stale_oid).is_ok());
        assert!(expired.read_object(&recent_oid).is_err());

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn sha1_expired_cruft_pack_survives_source_pruning() {
        expired_cruft_pack_survives_source_pruning(ObjectFormat::Sha1);
    }

    #[test]
    fn sha256_expired_cruft_pack_survives_source_pruning() {
        expired_cruft_pack_survives_source_pruning(ObjectFormat::Sha256);
    }

    #[test]
    fn reachable_repack_reuses_exact_single_pack_bytes() {
        let root = temp_root("sley-repack-reuse-exact-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, b"exact pack reuse\n");
        let commit_oid = graph[0].0;
        let objects = graph
            .iter()
            .map(|(_oid, object)| object.clone())
            .collect::<Vec<_>>();
        let undeltified = PackFile::write_undeltified(&objects, format)
            .expect("write deliberately undeltified source pack");
        db.install_pack(&undeltified).expect("install source pack");
        for (oid, _object) in &graph {
            fs::remove_file(db.loose().object_path(oid).expect("loose path"))
                .expect("remove duplicate loose object");
        }

        let result = repack_reachable_objects(&git_dir, format, &[commit_oid])
            .expect("repack exact reachable set")
            .expect("reachable objects exist");

        assert_eq!(result.pack, undeltified.pack);
        assert_eq!(result.idx, undeltified.index);
        assert_eq!(result.object_count, graph.len());
        assert!(result.obsolete_packs.is_empty());

        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn reachable_repack_does_not_reuse_pack_with_unreachable_object() {
        let root = temp_root("sley-repack-no-reuse-unreachable");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, b"reachable payload\n");
        let commit_oid = graph[0].0;
        let unreachable = EncodedObject::new(ObjectType::Blob, b"unreachable\n".to_vec());
        let mut objects = graph
            .iter()
            .map(|(_oid, object)| object.clone())
            .collect::<Vec<_>>();
        objects.push(unreachable);
        let source = PackFile::write_undeltified(&objects, format)
            .expect("write source pack containing unreachable object");
        db.install_pack(&source).expect("install source pack");
        for (oid, _object) in &graph {
            fs::remove_file(db.loose().object_path(oid).expect("loose path"))
                .expect("remove duplicate loose object");
        }

        let result = repack_reachable_objects(&git_dir, format, &[commit_oid])
            .expect("repack reachable subset")
            .expect("reachable objects exist");

        assert_eq!(result.object_count, graph.len());
        assert_ne!(result.pack, source.pack);
        assert_eq!(result.obsolete_packs.len(), 1);

        fs::remove_dir_all(root).expect("remove test repository");
    }

    #[test]
    fn reachable_repack_force_rewrite_bypasses_exact_pack_reuse() {
        let root = temp_root("sley-repack-force-rewrite-exact-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, b"forced rewrite\n");
        let commit_oid = graph[0].0;
        let mut objects = graph
            .iter()
            .map(|(_oid, object)| object.clone())
            .collect::<Vec<_>>();
        objects.reverse();
        let source = PackFile::write_undeltified(&objects, format)
            .expect("write source pack in reverse object order");
        db.install_pack(&source).expect("install source pack");
        for (oid, _object) in &graph {
            fs::remove_file(db.loose().object_path(oid).expect("loose path"))
                .expect("remove duplicate loose object");
        }
        let options = RepackOptions {
            force_rewrite: true,
            ..RepackOptions::default()
        };

        let result =
            repack_reachable_objects_with_options(&git_dir, format, &[commit_oid], &options)
                .expect("force rewrite reachable set")
                .expect("reachable objects exist");

        assert_eq!(result.object_count, graph.len());
        assert_ne!(result.pack, source.pack);
        assert_eq!(result.obsolete_packs.len(), 1);

        fs::remove_dir_all(root).expect("remove test repository");
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
    fn install_repack_bitmap_uses_retained_objects_and_writes_valid_index() {
        let root = temp_root("sley-repack-install-bitmap-cache");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let graph = write_commit_graph(&mut db, b"bitmap cache\n");
        let commit_oid = graph[0].0;

        let result = repack_all_objects(&git_dir, format)
            .expect("test operation should succeed")
            .expect("repository has objects");
        // Prove bitmap construction uses the objects retained by the repack
        // result: the pre-repack loose database is no longer available as a
        // fallback when the bitmap closure is walked.
        fs::remove_dir_all(git_dir.join("objects")).expect("remove loose database");
        fs::create_dir_all(git_dir.join("objects")).expect("recreate objects directory");
        install_repack_result_with_bitmap(
            &git_dir,
            format,
            &result,
            false,
            Some(&HashSet::from([commit_oid])),
            None,
        )
        .expect("test operation should succeed");

        let checksum = PackFile::parse(&result.pack, format)
            .expect("repacked objects are valid")
            .checksum;
        let bitmap_path = git_dir
            .join("objects")
            .join("pack")
            .join(format!("pack-{}.bitmap", checksum.to_hex()));
        let bitmap = fs::read(bitmap_path).expect("bitmap was written");
        let parsed = PackBitmapIndex::parse(&bitmap, format, result.object_count)
            .expect("bitmap index is valid");
        assert!(
            !parsed.entries.is_empty(),
            "the commit tip should receive a bitmap"
        );

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
            .install_pack_with_options(
                &promisor_pack,
                RawPackInstallOptions {
                    promisor: true,
                    ..Default::default()
                },
            )
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
