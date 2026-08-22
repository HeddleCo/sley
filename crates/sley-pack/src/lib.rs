// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use flate2::{Compress, Compression, FlushCompress, Status};
use sley_core::{
    ByteBudget, CancelFlag, GitError, ObjectFormat, ObjectId, ResourceLimitKind, Result,
    StreamingDigest,
};
use sley_formats::Bundle;
use sley_object::{EncodedObject, ObjectType};
use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::Write;
use std::ops::Range;
use std::sync::Arc;

// --- Mechanical module split (W21) -----------------------------------------
// The former single ~10k-line lib.rs is partitioned into contiguous
// submodules along its existing function-cluster seams. Each submodule pulls
// the crate-root scope in via `use super::*` and is re-exported below so every
// `sley_pack::X` path (public API and intra-crate) resolves unchanged.
// This is a pure code move: no function body was altered.
mod bounded_read;
mod delta;
mod fix_thin;
mod index;
pub mod inflate;
mod limits;
mod parallel_index;
mod read;
mod write;

pub use bounded_read::*;
pub(crate) use delta::*;
pub use fix_thin::*;
pub use index::*;
pub use limits::{MAX_READ_DELTA_CHAIN_DEPTH, PACK_OBJECT_COUNT_PREALLOC_CAP};
pub(crate) use limits::{checked_pack_object_count, pack_entry_prealloc};
pub use parallel_index::*;
pub use read::*;
pub use write::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    pub oid: ObjectId,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub offset: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackPolicy {
    pub write_bitmaps: bool,
    pub cruft_packs: bool,
    pub geometric_factor: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackFile {
    pub version: u32,
    pub entries: Vec<PackObject>,
    pub checksum: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackObject {
    pub entry: PackEntry,
    pub object: EncodedObject,
}

/// Per-object statistics for one entry of a verified pack, in the shape
/// `git verify-pack -v` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackVerifyStat {
    /// Resolved object id.
    pub oid: ObjectId,
    /// Resolved object type (the delta's *result* type, not `ofs-delta`).
    pub object_type: ObjectType,
    /// Resolved (inflated) object size in bytes.
    pub size: u64,
    /// Bytes this object occupies in the pack: the offset delta to the next
    /// object, or to the trailing checksum for the last object.
    pub size_in_pack: u64,
    /// In-pack byte offset where this object's entry begins.
    pub offset: u64,
    /// Delta chain depth: `0` for undeltified objects, base-depth + 1 otherwise.
    pub delta_depth: u32,
    /// For delta objects, the id of the *immediate* base object (which may
    /// itself be a delta). `None` for undeltified objects.
    pub base_oid: Option<ObjectId>,
}

/// Result of [`PackFile::verify_pack_stats`]: per-object stats in pack offset
/// order plus the pack's trailing checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackVerifyStats {
    pub objects: Vec<PackVerifyStat>,
    pub checksum: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackWrite {
    pub pack: Vec<u8>,
    pub index: Vec<u8>,
    pub checksum: ObjectId,
    pub entries: Vec<PackIndexEntry>,
    pub delta_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackWriteSummary {
    pub index: Vec<u8>,
    pub checksum: ObjectId,
    pub entries: Vec<PackIndexEntry>,
    pub delta_count: u32,
    pub pack_size: u64,
    /// High-water mark of decoded window bodies plus retained delta bases,
    /// as charged by [`PackWriteLimits`]. Does not include zlib output buffers
    /// or allocator slack.
    pub peak_working_set_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackInput<'a> {
    pub oid: &'a ObjectId,
    pub object: &'a EncodedObject,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackObjectKind {
    Commit,
    Tree,
    Blob,
    Tag,
    OfsDelta,
    RefDelta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeltaBase {
    Offset(u64),
    Ref(ObjectId),
}

/// One pack entry as stored on disk, used by [`PackFile::verify_pack_stats`] to
/// recover the delta structure and on-disk stream size that resolved
/// [`PackObject`]s no longer carry.
struct OnDiskEntry {
    offset: u64,
    base: Option<DeltaBase>,
    stream_size: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntryHeader {
    kind: PackObjectKind,
    size: u64,
}
fn next_byte(bytes: &[u8], offset: &mut usize) -> Result<u8> {
    let Some(byte) = bytes.get(*offset).copied() else {
        return Err(GitError::InvalidFormat(
            "truncated pack entry header".into(),
        ));
    };
    *offset += 1;
    Ok(byte)
}

fn u16_be(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}
fn checked_range(
    start: usize,
    count: usize,
    width: usize,
    total: usize,
) -> Result<std::ops::Range<usize>> {
    let len = count
        .checked_mul(width)
        .ok_or_else(|| GitError::InvalidFormat("pack index table overflow".into()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| GitError::InvalidFormat("pack index table overflow".into()))?;
    if end > total {
        return Err(GitError::InvalidFormat("truncated pack index table".into()));
    }
    Ok(start..end)
}

fn validate_position_permutation(positions: &[u32]) -> Result<()> {
    let mut seen = vec![false; positions.len()];
    for position in positions {
        let idx = *position as usize;
        if idx >= positions.len() {
            return Err(GitError::InvalidFormat(format!(
                "invalid rev-index position {position}"
            )));
        }
        if seen[idx] {
            return Err(GitError::InvalidFormat(format!(
                "invalid rev-index position {position}"
            )));
        }
        seen[idx] = true;
    }
    Ok(())
}

// Reused zlib inflate state. Resetting and reusing one `Decompress` avoids
// allocating a fresh (~10 KiB) `InflateState` for every object and delta decoded —
// an allocation that dominated bulk reads. Borrowed only for the duration of a
// single inflate; the recursive pack reader fully inflates each entry's data before
// recursing to its base, so the borrow never nests.
thread_local! {
    pub(crate) static INFLATE: RefCell<flate2::Decompress> = RefCell::new(flate2::Decompress::new(true));
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::read::ZlibDecoder;
    use flate2::write::ZlibEncoder;
    use sley_core::AtomicCancel;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn multi_blob_pack(count: usize) -> Vec<u8> {
        let objects = (0..count)
            .map(|idx| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("stream cancel pack object {idx}\n").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        PackFile::write_undeltified(&objects, ObjectFormat::Sha1)
            .expect("test operation should succeed")
            .pack
    }

    #[test]
    fn parallel_index_already_cancelled_returns_cancelled() {
        let pack = multi_blob_pack(4);
        let source = AtomicCancel::new();
        source.cancel();
        let err = PackIndex::write_v2_for_pack_with_options(
            &pack,
            ObjectFormat::Sha1,
            |_| Ok(None),
            PackIndexOptions::default(),
            CancelFlag::new(&source),
            |_| {},
        )
        .expect_err("pre-cancelled index should fail");
        assert_eq!(err, GitError::Cancelled);
    }

    #[test]
    fn parallel_index_is_identical_with_one_or_many_workers() {
        let pack = multi_blob_pack(128);
        let serial_schedule = PackIndex::write_v2_for_pack_with_options(
            &pack,
            ObjectFormat::Sha1,
            |_| Ok(None),
            PackIndexOptions::default().with_threads(1),
            CancelFlag::never(),
            |_| {},
        )
        .expect("one-worker schedule");
        let parallel_schedule = PackIndex::write_v2_for_pack_with_options(
            &pack,
            ObjectFormat::Sha1,
            |_| Ok(None),
            PackIndexOptions::default().with_threads(32),
            CancelFlag::never(),
            |_| {},
        )
        .expect("many-worker schedule");
        assert_eq!(parallel_schedule, serial_schedule);
    }

    #[test]
    fn write_packed_from_source_respects_cancel_between_windows() {
        let format = ObjectFormat::Sha1;
        let count = 4u32;
        let objects = (0..count)
            .map(|idx| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("write-cancel object {idx}\n").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        let object_ids = objects
            .iter()
            .map(|object| object.object_id(format).expect("oid"))
            .collect::<Vec<_>>();
        let object_map = object_ids
            .iter()
            .copied()
            .zip(objects.into_iter().map(Arc::new))
            .collect::<HashMap<_, _>>();

        let source = AtomicCancel::new();
        source.cancel();
        let mut written = Vec::new();
        let err = PackFile::write_packed_from_source_to_writer_with_cancel(
            object_ids.iter().copied(),
            count,
            format,
            &PackWriteOptions::new().with_reorder(false),
            PackWriteLimits::default(),
            |oid| {
                object_map
                    .get(oid)
                    .cloned()
                    .ok_or_else(|| GitError::not_found(format!("missing test object {oid}")))
            },
            &mut written,
            CancelFlag::new(&source),
        )
        .expect_err("pre-cancelled pack write should fail");
        assert_eq!(err, GitError::Cancelled);
        assert!(
            written.is_empty() || written.len() < 32,
            "should not have finished a full pack after cancel"
        );
    }

    fn delta_pack_options(prefer_ofs_delta: bool) -> PackWriteOptions {
        PackWriteOptions::new()
            .with_prefer_ofs_delta(prefer_ofs_delta)
            .with_reorder(false)
    }

    #[test]
    fn parses_single_blob_pack() {
        let pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"hello\n");
        let parsed = PackFile::parse_sha1(&pack).expect("test operation should succeed");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.entries.len(), 1);
        let object = &parsed.entries[0].object;
        assert_eq!(object.object_type, ObjectType::Blob);
        assert_eq!(object.body, b"hello\n");
        assert_eq!(
            parsed.entries[0].entry.oid.to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn parses_single_blob_pack_sha256() {
        let pack = single_object_pack(ObjectFormat::Sha256, ObjectType::Blob, b"hello\n");
        let parsed =
            PackFile::parse(&pack, ObjectFormat::Sha256).expect("test operation should succeed");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.entries.len(), 1);
        let object = &parsed.entries[0].object;
        assert_eq!(object.object_type, ObjectType::Blob);
        assert_eq!(object.body, b"hello\n");
        assert_eq!(
            parsed.entries[0].entry.oid,
            object
                .object_id(ObjectFormat::Sha256)
                .expect("test operation should succeed")
        );
    }

    #[test]
    fn parses_bundle_pack_payload_with_bundle_format() {
        let pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"bundle\n");
        let oid = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"bundle\n")
            .expect("test operation should succeed");
        let bundle_bytes = format!("# v2 git bundle\n{oid} refs/heads/main\n\n")
            .into_bytes()
            .into_iter()
            .chain(pack)
            .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        let parsed = PackFile::parse_bundle(&bundle).expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].object.object_type, ObjectType::Blob);
        assert_eq!(parsed.entries[0].object.body, b"bundle\n");
    }

    /// Build a pack whose single blob entry header LIES about its decompressed
    /// size: it declares `declared_size` while the actual zlib payload only
    /// inflates to `real_body`. A short `real_body` plus a `declared_size` of
    /// `u64::MAX` is the decompression-bomb shape — the header claims terabytes
    /// from a handful of compressed bytes.
    fn lying_size_blob_pack(format: ObjectFormat, declared_size: u64, real_body: &[u8]) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        // Object type 3 == blob; size varint encodes the *attacker-declared* size.
        write_pack_entry_header_kind(&mut pack, 3, declared_size);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(real_body)
            .expect("test operation should succeed");
        pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));
        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    /// Regression: a crafted pack object header declaring a gigantic decompressed
    /// size with a tiny compressed payload must NOT drive an up-front
    /// reservation/allocation of that declared size (OOM/abort). sley#2: the
    /// header `size` is attacker-controlled over the network (install_raw_pack →
    /// sley-fetch), so it must be validated/bounded before any `Vec::reserve`.
    ///
    /// On the unfixed code, `inflate_into` did `out.reserve(header.size as usize)`
    /// with `header.size == u64::MAX`, which panics with "capacity overflow" (or
    /// aborts on alloc failure) *before* the size-mismatch check could fire. We
    /// run parse on a worker thread so that panic surfaces as a `join()` error
    /// rather than killing the test process; the fix turns this into a clean
    /// `Err` returned normally.
    #[test]
    fn rejects_decompression_bomb_header_without_oom() {
        for &declared in &[u64::MAX, 100 * 1024 * 1024 * 1024, u64::from(u32::MAX) * 4] {
            let pack = lying_size_blob_pack(ObjectFormat::Sha1, declared, b"tiny\n");
            let handle = std::thread::spawn(move || PackFile::parse_sha1(&pack));
            let result = handle.join();
            // The parse thread must not have panicked/aborted on a huge reserve.
            assert!(
                result.is_ok(),
                "parsing a bomb header (declared={declared}) panicked instead of erroring cleanly"
            );
            // And parsing must reject the lie (decoded len != declared size).
            let parse_result = result.expect("parse thread should not panic on a bomb header");
            assert!(
                parse_result.is_err(),
                "bomb header (declared={declared}) should be rejected as invalid"
            );
        }
    }

    /// Build a 2-object pack: a real base blob followed by a delta (ref or ofs)
    /// whose *result-size* varint lies, declaring `declared_result_size`, while
    /// carrying a tiny real instruction stream. The delta's base-size varint is
    /// set correctly (so the base-size check at the top of `apply_pack_delta`
    /// passes and we reach the result reservation). Used to drive the sley#35
    /// delta-result-size bomb.
    fn lying_result_size_delta_pack(
        format: ObjectFormat,
        declared_result_size: u64,
        delta_kind: DeltaKind,
    ) -> Vec<u8> {
        let base = b"hello";
        let result = b"hello world"; // real produced length = 11

        // Hand-build a delta with a truthful base-size and a LYING result-size.
        let mut delta = Vec::new();
        write_delta_varint(&mut delta, base.len() as u64);
        write_delta_varint(&mut delta, declared_result_size);
        // Real instructions: copy `base` then insert " world".
        let suffix = &result[base.len()..];
        delta.push(0x90); // copy, 1 size byte present (bit 0x10)
        delta.push(base.len() as u8);
        delta.push(suffix.len() as u8);
        delta.extend_from_slice(suffix);

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&2u32.to_be_bytes());

        let base_offset = pack.len();
        write_entry_header(&mut pack, ObjectType::Blob, base.len() as u64);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(base)
            .expect("test operation should succeed");
        pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));

        let delta_offset = pack.len();
        write_pack_entry_header_kind(
            &mut pack,
            match delta_kind {
                DeltaKind::Offset => 6,
                DeltaKind::Ref => 7,
            },
            delta.len() as u64,
        );
        match delta_kind {
            DeltaKind::Offset => write_ofs_delta_offset(&mut pack, delta_offset - base_offset),
            DeltaKind::Ref => {
                let base_oid = sley_core::object_id_for_bytes(format, "blob", base)
                    .expect("test operation should succeed");
                pack.extend_from_slice(base_oid.as_bytes());
            }
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&delta)
            .expect("test operation should succeed");
        pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));

        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    /// Regression (sley#35): the 2nd instance of the sley#2 decompression-bomb
    /// class. `apply_pack_delta` read an attacker-controlled `result_size` varint
    /// from a network delta and fed it straight to `Vec::with_capacity`. A tiny
    /// delta declaring `result_size == u64::MAX` (or ~1 TiB) aborts the process
    /// ("capacity overflow"/alloc failure, SIGABRT) BEFORE the post-decode
    /// size-mismatch check can reject the lie. Both ref-delta and ofs-delta paths
    /// reach the same reservation, so both must be safe. We resolve the pack on a
    /// worker thread so an abort/panic surfaces as a `join()` error rather than
    /// killing the whole test binary; the fix turns the bomb into a clean `Err`.
    #[test]
    fn rejects_delta_result_size_bomb_without_oom() {
        let bombs: &[u64] = &[u64::MAX, 1024 * 1024 * 1024 * 1024];
        for &declared in bombs {
            for delta_kind in [DeltaKind::Ref, DeltaKind::Offset] {
                let pack = lying_result_size_delta_pack(ObjectFormat::Sha1, declared, delta_kind);
                let handle = std::thread::spawn(move || PackFile::parse_sha1(&pack));
                let join_result = handle.join();
                assert!(
                    join_result.is_ok(),
                    "delta bomb (declared={declared}, kind={delta_kind:?}) panicked/aborted \
                     instead of erroring cleanly"
                );
                let parse_result =
                    join_result.expect("parse thread should not panic on a delta bomb");
                assert!(
                    parse_result.is_err(),
                    "delta bomb (declared={declared}, kind={delta_kind:?}) should be rejected \
                     as invalid (result.len() != declared)"
                );
            }
        }
    }

    /// A legitimate (truthful) delta whose result-size varint matches the real
    /// produced length must still resolve correctly — the bound only caps the
    /// speculative reservation, it must not break real delta application.
    #[test]
    fn applies_legitimate_delta_after_result_size_bound() {
        for delta_kind in [DeltaKind::Ref, DeltaKind::Offset] {
            let base = b"hello";
            let result = b"hello world";
            let pack = two_object_delta_pack(ObjectFormat::Sha1, base, result, delta_kind);
            let parsed = PackFile::parse_sha1(&pack).expect("legitimate delta should resolve");
            assert_eq!(parsed.entries.len(), 2);
            assert_eq!(parsed.entries[0].object.body, base);
            assert_eq!(parsed.entries[1].object.body, result);
        }
    }

    #[test]
    fn bounded_delta_application_polls_cancel_between_commands() {
        let base = vec![b'x'; 4096];
        let mut result = base.clone();
        result.extend_from_slice(b"changed");
        let delta = DeltaIndex::new(&base).delta(&result).expect("delta");
        let plan = plan_pack_delta(&base, &delta).expect("plan");
        let mut out = Vec::with_capacity(result.len());
        let source = AtomicCancel::new();
        source.cancel();

        assert_eq!(
            apply_pack_delta_exact(&base, &delta, plan, &mut out, CancelFlag::new(&source)),
            Err(GitError::Cancelled)
        );
        assert!(out.is_empty());
    }

    #[test]
    fn legacy_delta_preserves_final_size_mismatch_and_copy_heavy_growth() {
        let base = b"x";
        let overproducing = vec![1, 1, 2, b'x', b'y'];
        assert_eq!(
            apply_pack_delta(base, &overproducing),
            Err(GitError::InvalidObject(
                "delta result size mismatch: expected 1, got 2".into()
            ))
        );

        let copies = 4096usize;
        let mut copy_heavy = Vec::new();
        write_delta_varint(&mut copy_heavy, base.len() as u64);
        write_delta_varint(&mut copy_heavy, copies as u64);
        for _ in 0..copies {
            // Copy one byte from base offset zero.
            copy_heavy.extend_from_slice(&[0x91, 0, 1]);
        }
        assert_eq!(
            apply_pack_delta(base, &copy_heavy).expect("copy-heavy legacy delta"),
            vec![b'x'; copies]
        );
    }

    #[test]
    fn rejects_bundle_pack_payload_with_wrong_object_format() {
        let pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"bundle\n");
        let oid = sley_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"bundle\n")
            .expect("test operation should succeed");
        let bundle_bytes =
            format!("# v3 git bundle\n@object-format=sha256\n{oid} refs/heads/main\n\n")
                .into_bytes()
                .into_iter()
                .chain(pack)
                .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        assert!(PackFile::parse_bundle(&bundle).is_err());
    }

    fn assert_pack_index_view_matches_owned(index: &[u8], format: ObjectFormat) {
        let owned = PackIndex::parse(index, format).expect("test operation should succeed");
        let view = PackIndexView::parse(index, format).expect("test operation should succeed");
        let owned_view =
            PackIndexViewData::parse(Arc::from(index.to_vec().into_boxed_slice()), format)
                .expect("test operation should succeed");

        assert_eq!(view.version, owned.version);
        assert_eq!(view.count, owned.entries.len());
        assert_eq!(view.count(), owned.entries.len());
        assert_eq!(view.fanout(), &owned.fanout);
        assert_eq!(view.pack_checksum, owned.pack_checksum);
        assert_eq!(view.index_checksum, owned.index_checksum);
        assert_eq!(owned_view.version, owned.version);
        assert_eq!(owned_view.count(), owned.entries.len());
        assert_eq!(owned_view.fanout(), &owned.fanout);
        assert_eq!(owned_view.pack_checksum, owned.pack_checksum);
        assert_eq!(owned_view.index_checksum, owned.index_checksum);
        for entry in &owned.entries {
            let owned_found = owned
                .find(&entry.oid)
                .expect("test operation should succeed");
            let expected = Some(PackIndexLookup {
                crc32: owned_found.crc32,
                offset: owned_found.offset,
            });
            assert_eq!(view.find(&entry.oid), expected);
            assert_eq!(owned_view.find(&entry.oid), expected);
        }
    }

    #[test]
    fn writes_pack_and_index_that_round_trip() {
        let object = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec());
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object))
            .expect("test operation should succeed");
        let pack = PackFile::parse_sha1(&written.pack).expect("test operation should succeed");
        let index =
            PackIndex::parse_v2_sha1(&written.index).expect("test operation should succeed");
        let oid = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(pack.entries[0].object, object);
        assert_eq!(index.pack_checksum, pack.checksum);
        assert_eq!(
            index
                .find(&oid)
                .expect("test operation should succeed")
                .offset,
            12
        );
    }

    #[test]
    fn pack_index_view_matches_owned_index_for_generated_sha1_pack() {
        let objects = (0..8)
            .map(|idx| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("borrowed pack index view sha1 object {idx}\n").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        let written = PackFile::write_packed(&objects, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        assert_pack_index_view_matches_owned(&written.index, ObjectFormat::Sha1);

        let view =
            PackIndexView::parse_v2_sha1(&written.index).expect("test operation should succeed");
        let missing = sley_core::object_id_for_bytes(
            ObjectFormat::Sha1,
            "blob",
            b"not present in borrowed index\n",
        )
        .expect("test operation should succeed");
        assert_eq!(view.find(&missing), None);
    }

    #[test]
    fn writes_sha256_pack_and_index_that_round_trip() {
        let object = EncodedObject::new(ObjectType::Blob, b"hello sha256\n".to_vec());
        let written =
            PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
                .expect("test operation should succeed");
        let pack = PackFile::parse(&written.pack, ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let index = PackIndex::parse(&written.index, ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let oid = object
            .object_id(ObjectFormat::Sha256)
            .expect("test operation should succeed");
        assert_eq!(pack.entries[0].object, object);
        assert_eq!(index.pack_checksum, pack.checksum);
        assert_eq!(index.pack_checksum.format(), ObjectFormat::Sha256);
        assert_eq!(index.index_checksum.format(), ObjectFormat::Sha256);
        assert_eq!(
            index
                .find(&oid)
                .expect("test operation should succeed")
                .offset,
            12
        );
    }

    #[test]
    fn pack_index_view_matches_owned_index_for_generated_sha256_pack() {
        let objects = (0..4)
            .map(|idx| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("borrowed pack index view sha256 object {idx}\n").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        let written = PackFile::write_undeltified(&objects, ObjectFormat::Sha256)
            .expect("test operation should succeed");

        assert_pack_index_view_matches_owned(&written.index, ObjectFormat::Sha256);
    }

    #[test]
    fn indexes_existing_sha256_pack_bytes() {
        let object = EncodedObject::new(ObjectType::Blob, b"index raw sha256 pack\n".to_vec());
        let written =
            PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
                .expect("test operation should succeed");

        let indexed = PackIndex::write_v2_for_pack(&written.pack, ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let index = PackIndex::parse(&indexed.index, ObjectFormat::Sha256)
            .expect("test operation should succeed");

        assert_eq!(indexed.pack_checksum, written.checksum);
        assert_eq!(indexed.entries, written.entries);
        assert_eq!(index.pack_checksum, written.checksum);
        assert_eq!(index.entries, written.entries);
    }

    #[test]
    fn indexes_existing_delta_pack_bytes() {
        let (base, changed) = similar_blob_objects();
        let options = delta_pack_options(true);
        let written = PackFile::write_packed_with_options(
            &[base, changed.clone()],
            ObjectFormat::Sha1,
            &options,
        )
        .expect("test operation should succeed");

        let indexed = PackIndex::write_v2_for_pack(&written.pack, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let index =
            PackIndex::parse_v2_sha1(&indexed.index).expect("test operation should succeed");
        let changed_oid = changed
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        assert_eq!(indexed.pack_checksum, written.checksum);
        assert_eq!(indexed.entries, written.entries);
        assert_eq!(
            index
                .find(&changed_oid)
                .expect("test operation should succeed")
                .offset,
            written.entries[1].offset
        );
        assert_eq!(
            index
                .find(&changed_oid)
                .expect("test operation should succeed")
                .crc32,
            written.entries[1].crc32
        );
    }

    #[test]
    fn git_generated_pack_is_identical_with_one_and_many_workers() {
        let root = unique_temp_dir("parallel-git-pack");
        let repository = root.join("repo");
        fs::create_dir_all(&repository).expect("create git fixture directory");
        run_git_success(&repository, &["init", "-q"]);
        run_git_success(&repository, &["config", "user.name", "Sley Test"]);
        run_git_success(
            &repository,
            &["config", "user.email", "sley@example.invalid"],
        );
        for index in 0..64 {
            let body = format!("git generated pack object {index}\n").repeat(128);
            fs::write(repository.join(format!("object-{index:03}.txt")), body)
                .expect("write git fixture object");
        }
        run_git_success(&repository, &["add", "."]);
        run_git_success(&repository, &["commit", "-q", "-m", "pack fixture"]);
        for index in 0..64 {
            let path = repository.join(format!("object-{index:03}.txt"));
            let mut body = fs::read(&path).expect("read git fixture object");
            body.extend_from_slice(format!("second version {index}\n").as_bytes());
            fs::write(path, body).expect("update git fixture object");
        }
        run_git_success(&repository, &["add", "."]);
        run_git_success(&repository, &["commit", "-q", "-m", "deltified fixture"]);
        run_git_success(&repository, &["gc", "--aggressive", "--prune=now"]);

        let pack_dir = repository.join(".git/objects/pack");
        let pack_path = largest_path_with_extension(&pack_dir, "pack");
        let pack = fs::read(&pack_path).expect("read git-generated pack");
        let git_index = fs::read(pack_path.with_extension("idx")).expect("read git index");
        let one = PackIndex::write_v2_for_pack_with_options(
            &pack,
            ObjectFormat::Sha1,
            |_| Ok(None),
            PackIndexOptions::default().with_threads(1),
            CancelFlag::never(),
            |_| {},
        )
        .expect("index git pack with one worker");
        let many = PackIndex::write_v2_for_pack_with_options(
            &pack,
            ObjectFormat::Sha1,
            |_| Ok(None),
            PackIndexOptions::default().with_threads(64),
            CancelFlag::never(),
            |_| {},
        )
        .expect("index git pack with many workers");
        assert_eq!(many, one);
        assert_eq!(many.index, git_index);
        fs::remove_dir_all(root).expect("remove git fixture");
    }

    #[test]
    fn parallel_index_rejects_self_referential_ref_delta_with_valid_checksum() {
        let format = ObjectFormat::Sha1;
        let mut pack = ref_delta_chain_pack(format, 1, false);
        let descriptors = pack_entry_descriptors(&pack, format);
        let delta_offset = descriptors[1].offset as usize;
        let mut base_oid_offset = delta_offset;
        let header = parse_entry_header(&pack, &mut base_oid_offset).expect("delta header");
        assert_eq!(header.kind, PackObjectKind::RefDelta);
        let self_oid = sley_core::object_id_for_bytes(format, "blob", &chain_bodies(1)[1])
            .expect("self object id");
        let base_oid_end = base_oid_offset + format.raw_len();
        pack[base_oid_offset..base_oid_end].copy_from_slice(self_oid.as_bytes());
        let trailer_offset = pack.len() - format.raw_len();
        let checksum = sley_core::digest_bytes(format, &pack[..trailer_offset])
            .expect("recompute valid pack checksum");
        pack[trailer_offset..].copy_from_slice(checksum.as_bytes());

        let error = PackIndex::write_v2_for_pack_with_options(
            &pack,
            format,
            |_| Ok(None),
            PackIndexOptions::default().with_threads(32),
            CancelFlag::never(),
            |_| {},
        )
        .expect_err("a self-referential delta chain must be rejected");
        assert!(
            error.to_string().contains("cyclic") || error.to_string().contains("unresolved"),
            "unexpected chain error: {error}"
        );
    }

    #[test]
    fn writes_ref_delta_pack_and_index_that_round_trip() {
        let (base, changed) = similar_blob_objects();
        let options = delta_pack_options(false);
        let written = PackFile::write_packed_with_options(
            &[base.clone(), changed.clone()],
            ObjectFormat::Sha1,
            &options,
        )
        .expect("test operation should succeed");
        let mut second_offset = written.entries[1].offset as usize;
        let header = parse_entry_header(&written.pack, &mut second_offset)
            .expect("test operation should succeed");
        assert_eq!(header.kind, PackObjectKind::RefDelta);

        let pack = PackFile::parse_sha1(&written.pack).expect("test operation should succeed");
        let index =
            PackIndex::parse_v2_sha1(&written.index).expect("test operation should succeed");
        let oid = changed
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(pack.entries[0].object, base);
        assert_eq!(pack.entries[1].object, changed);
        assert_eq!(index.pack_checksum, pack.checksum);
        assert_eq!(
            index
                .find(&oid)
                .expect("test operation should succeed")
                .offset,
            written.entries[1].offset
        );
    }

    #[test]
    fn read_object_at_matches_full_parse_for_ofs_delta_pack() {
        let (base, changed) = similar_blob_objects();
        let options = delta_pack_options(true);
        let written =
            PackFile::write_packed_with_options(&[base, changed], ObjectFormat::Sha1, &options)
                .expect("test operation should succeed");
        // Ensure the pack genuinely contains an ofs-delta (else the test is vacuous).
        let mut second = written.entries[1].offset as usize;
        assert_eq!(
            parse_entry_header(&written.pack, &mut second)
                .expect("test operation should succeed")
                .kind,
            PackObjectKind::OfsDelta
        );
        // Ground truth from a full parse; single-object decode must match at every offset.
        let parsed = PackFile::parse_sha1(&written.pack).expect("test operation should succeed");
        for po in &parsed.entries {
            let got =
                read_object_at_arc(&written.pack, po.entry.offset, ObjectFormat::Sha1, |_| {
                    Ok(None)
                })
                .expect("test operation should succeed");
            assert_eq!(*got, po.object, "offset {}", po.entry.offset);
        }
    }

    /// A [`HeaderTypeCache`] over a plain map, for asserting the cached header
    /// read is byte-identical to the uncached one cold and warm (sley#26).
    #[derive(Default)]
    struct MapHeaderTypeCache(HashMap<u64, PackObjectHeader>);

    impl HeaderTypeCache for MapHeaderTypeCache {
        fn get(&self, pack_offset: u64) -> Option<PackObjectHeader> {
            self.0.get(&pack_offset).copied()
        }
        fn put(&mut self, pack_offset: u64, header: PackObjectHeader) {
            self.0.insert(pack_offset, header);
        }
    }

    #[test]
    fn read_object_header_at_cached_matches_uncached_cold_and_warm_for_ofs_delta() {
        let (base, changed) = similar_blob_objects();
        let options = delta_pack_options(true);
        let written =
            PackFile::write_packed_with_options(&[base, changed], ObjectFormat::Sha1, &options)
                .expect("test operation should succeed");
        // Ensure the pack genuinely contains an ofs-delta (else the test is vacuous).
        let mut second = written.entries[1].offset as usize;
        assert_eq!(
            parse_entry_header(&written.pack, &mut second)
                .expect("test operation should succeed")
                .kind,
            PackObjectKind::OfsDelta
        );

        let parsed = PackFile::parse_sha1(&written.pack).expect("test operation should succeed");
        let mut cache = MapHeaderTypeCache::default();
        for po in &parsed.entries {
            let uncached = read_object_header_at(
                &written.pack,
                po.entry.offset,
                ObjectFormat::Sha1,
                0,
                |_, _| Ok(None),
            )
            .expect("test operation should succeed");
            // Type inherited from the chain base; size is the inflated body length.
            assert_eq!(
                uncached.type_and_size(),
                (po.object.object_type, po.object.body.len() as u64),
                "uncached header at offset {}",
                po.entry.offset
            );
            // Cold cache: must agree with the uncached read and populate the memo.
            let cold = read_object_header_at_with_cache(
                &written.pack,
                po.entry.offset,
                ObjectFormat::Sha1,
                0,
                |_, _| Ok(None),
                &mut cache,
            )
            .expect("test operation should succeed");
            assert_eq!(cold, uncached, "cold cache at offset {}", po.entry.offset);
        }
        // Warm cache: every offset now resolves from the memo and is still correct,
        // proving the fast path does not change behavior (sley#26).
        for po in &parsed.entries {
            let warm = read_object_header_at_with_cache(
                &written.pack,
                po.entry.offset,
                ObjectFormat::Sha1,
                0,
                |_, _| panic!("warm cache must not re-walk the chain"),
                &mut cache,
            )
            .expect("test operation should succeed");
            assert_eq!(
                warm.type_and_size(),
                (po.object.object_type, po.object.body.len() as u64),
                "warm cache at offset {}",
                po.entry.offset
            );
        }
    }

    #[test]
    fn read_object_at_matches_full_parse_for_ref_delta_pack() {
        let (base, changed) = similar_blob_objects();
        let options = delta_pack_options(false);
        let written =
            PackFile::write_packed_with_options(&[base, changed], ObjectFormat::Sha1, &options)
                .expect("test operation should succeed");
        let parsed = PackFile::parse_sha1(&written.pack).expect("test operation should succeed");
        let by_oid: HashMap<ObjectId, Arc<EncodedObject>> = parsed
            .entries
            .iter()
            .map(|po| (po.entry.oid, Arc::new(po.object.clone())))
            .collect();
        for po in &parsed.entries {
            let got =
                read_object_at_arc(&written.pack, po.entry.offset, ObjectFormat::Sha1, |oid| {
                    Ok(by_oid.get(oid).cloned())
                })
                .expect("test operation should succeed");
            assert_eq!(*got, po.object);
        }
    }

    /// A test-only [`PackDeltaCache`] that records every decode and counts hits,
    /// used to prove the cached decode path is byte-identical to the uncached
    /// one and that bases are reused across reads.
    #[derive(Default)]
    struct CountingDeltaCache {
        map: std::cell::RefCell<HashMap<u64, Arc<EncodedObject>>>,
        hits: std::cell::Cell<usize>,
        inserts: std::cell::Cell<usize>,
    }

    impl PackDeltaCache for CountingDeltaCache {
        fn get(&self, offset: u64) -> Option<Arc<EncodedObject>> {
            let hit = self.map.borrow().get(&offset).cloned();
            if hit.is_some() {
                self.hits.set(self.hits.get() + 1);
            }
            hit
        }
        fn insert(&self, offset: u64, object: Arc<EncodedObject>) {
            self.inserts.set(self.inserts.get() + 1);
            self.map.borrow_mut().insert(offset, object);
        }
    }

    #[test]
    fn read_object_at_with_cache_matches_uncached_and_reuses_bases() {
        // A multi-object pack with a real ofs-delta chain so the cache has bases
        // to reuse. Build several similar blobs to encourage deltification.
        let mut objects = Vec::new();
        for idx in 0..8u32 {
            let mut body = vec![b'x'; 4096];
            body.extend_from_slice(format!("\nvariant {idx}\n").as_bytes());
            objects.push(EncodedObject::new(ObjectType::Blob, body));
        }
        let options = delta_pack_options(true);
        let written = PackFile::write_packed_with_options(&objects, ObjectFormat::Sha1, &options)
            .expect("test operation should succeed");
        let parsed = PackFile::parse_sha1(&written.pack).expect("test operation should succeed");

        let cache = CountingDeltaCache::default();
        // Read every object twice through the cache; each result must equal the
        // ground-truth from the full parse, byte for byte, both times.
        for _ in 0..2 {
            for po in &parsed.entries {
                let got = read_object_at_with_cache_arc(
                    &written.pack,
                    po.entry.offset,
                    ObjectFormat::Sha1,
                    |_| Ok(None),
                    &cache,
                )
                .expect("test operation should succeed");
                assert_eq!(*got, po.object, "offset {}", po.entry.offset);
            }
        }
        // The second pass reads everything straight from the cache, so there must
        // be at least one hit (proving reuse, not just correctness).
        assert!(cache.hits.get() > 0, "cache never served a warm object");
    }

    #[test]
    fn writes_ofs_delta_pack_and_index_that_round_trip() {
        let (base, changed) = similar_blob_objects();
        let options = delta_pack_options(true);
        let written = PackFile::write_packed_with_options(
            &[base.clone(), changed.clone()],
            ObjectFormat::Sha1,
            &options,
        )
        .expect("test operation should succeed");
        let mut second_offset = written.entries[1].offset as usize;
        let header = parse_entry_header(&written.pack, &mut second_offset)
            .expect("test operation should succeed");
        assert_eq!(header.kind, PackObjectKind::OfsDelta);

        let pack = PackFile::parse_sha1(&written.pack).expect("test operation should succeed");
        let index =
            PackIndex::parse_v2_sha1(&written.index).expect("test operation should succeed");
        let oid = changed
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(pack.entries[0].object, base);
        assert_eq!(pack.entries[1].object, changed);
        assert_eq!(index.pack_checksum, pack.checksum);
        assert_eq!(
            index
                .find(&oid)
                .expect("test operation should succeed")
                .offset,
            written.entries[1].offset
        );
    }

    #[test]
    fn resolves_ofs_delta_pack_entry() {
        let base = b"hello";
        let result = b"hello world";
        let pack = two_object_delta_pack(ObjectFormat::Sha1, base, result, DeltaKind::Offset);
        let parsed = PackFile::parse_sha1(&pack).expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].object.body, base);
        assert_eq!(parsed.entries[1].object.body, result);
        assert_eq!(
            parsed.entries[1].entry.oid,
            sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", result)
                .expect("test operation should succeed")
        );
    }

    #[test]
    fn resolves_ref_delta_pack_entry() {
        let base = b"hello";
        let result = b"hello world";
        let pack = two_object_delta_pack(ObjectFormat::Sha1, base, result, DeltaKind::Ref);
        let parsed = PackFile::parse_sha1(&pack).expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].object.body, base);
        assert_eq!(parsed.entries[1].object.body, result);
        assert_eq!(
            parsed.entries[1].entry.oid,
            sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", result)
                .expect("test operation should succeed")
        );
    }

    #[test]
    fn resolves_thin_ref_delta_pack_entry_with_external_base() {
        let base = b"hello";
        let result = b"hello world";
        let pack = thin_ref_delta_pack(ObjectFormat::Sha1, base, result);
        assert!(PackFile::parse_sha1(&pack).is_err());

        let base_oid = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", base)
            .expect("test operation should succeed");
        let parsed = PackFile::parse_thin(&pack, ObjectFormat::Sha1, |oid| {
            if oid == &base_oid {
                Ok(Some(EncodedObject::new(ObjectType::Blob, base.to_vec())))
            } else {
                Ok(None)
            }
        })
        .expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].object.body, result);
        assert_eq!(
            parsed.entries[0].entry.oid,
            sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", result)
                .expect("test operation should succeed")
        );
    }

    #[test]
    fn fixes_thin_pack_and_indexes_it_without_external_bases() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let base = b"hello";
            let result = b"hello world";
            let pack = thin_ref_delta_pack(format, base, result);
            let base_object = EncodedObject::new(ObjectType::Blob, base.to_vec());
            let base_oid = base_object
                .object_id(format)
                .expect("test operation should succeed");

            let fixed = fix_thin_pack(&pack, format, |oid| {
                Ok((oid == &base_oid).then(|| base_object.clone()))
            })
            .expect("thin pack completion should succeed");

            assert_eq!(u32_be(&fixed.pack[8..12]), 2);
            assert_eq!(fixed.appended_bases, vec![base_oid]);
            assert_eq!(fixed.index.entries.len(), 2);
            let parsed =
                PackFile::parse(&fixed.pack, format).expect("completed pack should stand alone");
            assert_eq!(parsed.entries.len(), 2);
            assert_eq!(parsed.entries[0].object.body, result);
            assert_eq!(parsed.entries[1].object, base_object);
            let rebuilt = PackIndex::write_v2_for_pack(&fixed.pack, format)
                .expect("completed pack should index without a resolver");
            assert_eq!(rebuilt, fixed.index);
        }
    }

    #[test]
    fn fix_thin_is_idempotent_for_a_complete_pack() {
        let original = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"complete\n");
        let mut resolver_called = false;
        let fixed = fix_thin_pack(&original, ObjectFormat::Sha1, |_| {
            resolver_called = true;
            Ok(None)
        })
        .expect("complete pack should remain complete");

        assert_eq!(fixed.pack, original);
        assert!(fixed.appended_bases.is_empty());
        assert!(!resolver_called);
        assert_eq!(fixed.index.entries.len(), 1);
    }

    #[test]
    fn fix_thin_does_not_duplicate_a_base_already_in_the_body() {
        let pack =
            two_object_delta_pack(ObjectFormat::Sha1, b"hello", b"hello world", DeltaKind::Ref);
        let original_count = u32_be(&pack[8..12]);
        let mut resolver_called = false;
        let fixed = fix_thin_pack(&pack, ObjectFormat::Sha1, |_| {
            resolver_called = true;
            Ok(None)
        })
        .expect("in-pack ref base should remain self-contained");

        assert_eq!(fixed.pack, pack);
        assert_eq!(u32_be(&fixed.pack[8..12]), original_count);
        assert_eq!(fixed.index.entries.len(), original_count as usize);
        assert!(fixed.appended_bases.is_empty());
        assert!(!resolver_called);
    }

    #[test]
    fn fix_thin_appends_a_repeated_external_base_once() {
        let base = b"shared";
        let pack =
            thin_ref_delta_pack_many(ObjectFormat::Sha1, base, &[b"shared one", b"shared two"]);
        let base_object = EncodedObject::new(ObjectType::Blob, base.to_vec());
        let base_oid = base_object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut calls = 0;
        let fixed = fix_thin_pack(&pack, ObjectFormat::Sha1, |oid| {
            calls += 1;
            Ok((oid == &base_oid).then(|| base_object.clone()))
        })
        .expect("shared base completion should succeed");

        assert_eq!(calls, 1);
        assert_eq!(fixed.appended_bases, vec![base_oid]);
        assert_eq!(u32_be(&fixed.pack[8..12]), 3);
        assert_eq!(fixed.index.entries.len(), 3);
    }

    #[test]
    fn fix_thin_rejects_missing_and_wrong_external_bases() {
        let base = b"hello";
        let pack = thin_ref_delta_pack(ObjectFormat::Sha1, base, b"hello world");
        assert!(fix_thin_pack(&pack, ObjectFormat::Sha1, |_| Ok(None)).is_err());

        let wrong = EncodedObject::new(ObjectType::Blob, b"wrong".to_vec());
        let error = fix_thin_pack(&pack, ObjectFormat::Sha1, |_| Ok(Some(wrong.clone())))
            .expect_err("wrong base body must not be accepted for the requested oid");
        assert!(error.to_string().contains("resolved to object"));
    }

    #[test]
    fn fix_thin_preserves_the_delta_depth_limit_boundary() {
        let base = b"hello";
        let pack = thin_ref_delta_pack(ObjectFormat::Sha1, base, b"hello world");
        let base_object = EncodedObject::new(ObjectType::Blob, base.to_vec());
        let base_oid = base_object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let resolve = |oid: &ObjectId| Ok((oid == &base_oid).then(|| base_object.clone()));

        fix_thin_pack_with_limits(
            &pack,
            ObjectFormat::Sha1,
            resolve,
            PackReadLimits {
                max_delta_depth: 1,
                ..PackReadLimits::default()
            },
        )
        .expect("one delta must pass at the exact limit");

        let error = fix_thin_pack_with_limits(
            &pack,
            ObjectFormat::Sha1,
            resolve,
            PackReadLimits {
                max_delta_depth: 0,
                ..PackReadLimits::default()
            },
        )
        .expect_err("one delta must fail immediately above the configured limit");
        assert!(error.to_string().contains("observed depth 1"));
    }

    #[test]
    fn parallel_indexer_resolves_thin_bases_but_rejects_missing_ones() {
        let base = b"hello";
        let result = b"hello stream";
        let pack = thin_ref_delta_pack(ObjectFormat::Sha1, base, result);
        let base_object = EncodedObject::new(ObjectType::Blob, base.to_vec());
        let base_oid = base_object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        let built = PackIndex::write_v2_for_pack_with_base(&pack, ObjectFormat::Sha1, |oid| {
            Ok((oid == &base_oid).then(|| base_object.clone()))
        })
        .expect("parallel indexer should resolve the supplied base");
        assert_eq!(built.entries.len(), 1);
        assert_eq!(
            built.entries[0].oid,
            sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", result)
                .expect("test operation should succeed")
        );

        let error = PackIndex::write_v2_for_pack_with_base(&pack, ObjectFormat::Sha1, |_| Ok(None))
            .expect_err("an unresolved base must remain an error");
        assert!(error.to_string().contains("unresolved"));
    }

    #[test]
    fn fixed_pack_passes_upstream_git_and_supports_clone() {
        let format = ObjectFormat::Sha1;
        let base = EncodedObject::new(
            ObjectType::Blob,
            b"the original contents are deliberately similar\n".repeat(32),
        );
        let mut target_body = base.body.clone();
        target_body.extend_from_slice(b"completed thin pack\n");
        let target = EncodedObject::new(ObjectType::Blob, target_body);
        let target_oid = target
            .object_id(format)
            .expect("test operation should succeed");

        let mut tree_body = b"100644 payload\0".to_vec();
        tree_body.extend_from_slice(target_oid.as_bytes());
        let tree = EncodedObject::new(ObjectType::Tree, tree_body);
        let tree_oid = tree
            .object_id(format)
            .expect("test operation should succeed");
        let commit = EncodedObject::new(
            ObjectType::Commit,
            format!(
                "tree {tree_oid}\nauthor Sley <sley@example.com> 1 +0000\n\
                 committer Sley <sley@example.com> 1 +0000\n\nthin pack fixture\n"
            )
            .into_bytes(),
        );
        let commit_oid = commit
            .object_id(format)
            .expect("test operation should succeed");
        let base_oid = base
            .object_id(format)
            .expect("test operation should succeed");
        let thin = PackFile::write_thin(
            &[commit, tree, target],
            format,
            HashMap::from([(base_oid, base.clone())]),
        )
        .expect("write thin pack");
        assert!(thin.delta_count > 0, "fixture must really be thin");
        assert!(PackIndex::write_v2_for_pack(&thin.pack, format).is_err());

        let fixed = fix_thin_pack(&thin.pack, format, |oid| {
            Ok((oid == &base_oid).then(|| base.clone()))
        })
        .expect("fix thin pack");
        let root = unique_temp_dir("fix-thin-git");
        let strict_pack = root.join("strict.pack");
        fs::create_dir_all(&root).expect("create test root");
        fs::write(&strict_pack, &fixed.pack).expect("write strict pack");
        run_git_success(&root, &["index-pack", "--strict", "strict.pack"]);

        let complete_repo = root.join("complete.git");
        run_git_success(
            &root,
            &[
                "init",
                "--bare",
                "-q",
                complete_repo.to_str().expect("utf8 complete repo path"),
            ],
        );
        let complete_pack_dir = complete_repo.join("objects/pack");
        let complete_stem = format!("pack-{}", fixed.index.pack_checksum);
        fs::write(
            complete_pack_dir.join(format!("{complete_stem}.pack")),
            &fixed.pack,
        )
        .expect("write completed repository pack");
        fs::write(
            complete_pack_dir.join(format!("{complete_stem}.idx")),
            &fixed.index.index,
        )
        .expect("write completed repository index");
        fs::create_dir_all(complete_repo.join("refs/heads")).expect("create refs");
        fs::write(
            complete_repo.join("refs/heads/main"),
            format!("{commit_oid}\n"),
        )
        .expect("write main ref");
        fs::write(complete_repo.join("HEAD"), b"ref: refs/heads/main\n").expect("write HEAD");
        run_git_success(&complete_repo, &["fsck", "--full"]);

        let clone_path = root.join("clone");
        run_git_success(
            &root,
            &[
                "clone",
                "--no-local",
                "-q",
                complete_repo.to_str().expect("utf8 complete repo path"),
                clone_path.to_str().expect("utf8 clone path"),
            ],
        );
        run_git_success(&clone_path, &["fsck", "--full"]);

        let thin_repo = root.join("thin.git");
        run_git_success(
            &root,
            &[
                "init",
                "--bare",
                "-q",
                thin_repo.to_str().expect("utf8 thin repo path"),
            ],
        );
        let thin_index = PackIndex::write_v2_for_pack_with_base(&thin.pack, format, |oid| {
            Ok((oid == &base_oid).then(|| base.clone()))
        })
        .expect("sley can build the deliberately permissive thin index");
        let thin_pack_dir = thin_repo.join("objects/pack");
        let thin_stem = format!("pack-{}", thin_index.pack_checksum);
        fs::write(thin_pack_dir.join(format!("{thin_stem}.pack")), &thin.pack)
            .expect("write thin repository pack");
        fs::write(
            thin_pack_dir.join(format!("{thin_stem}.idx")),
            &thin_index.index,
        )
        .expect("write thin repository index");
        fs::create_dir_all(thin_repo.join("refs/heads")).expect("create thin refs");
        fs::write(thin_repo.join("refs/heads/main"), format!("{commit_oid}\n"))
            .expect("write thin main ref");
        fs::write(thin_repo.join("HEAD"), b"ref: refs/heads/main\n").expect("write thin HEAD");
        let broken_clone_path = root.join("broken-clone");
        let failed_clone = Command::new("git")
            .current_dir(&root)
            .args([
                "clone",
                "--no-local",
                "-q",
                thin_repo.to_str().expect("utf8 thin repo path"),
                broken_clone_path.to_str().expect("utf8 broken clone path"),
            ])
            .output()
            .expect("run failing clone");
        assert!(
            !failed_clone.status.success(),
            "clone from uncompleted thin pack unexpectedly succeeded"
        );

        fs::remove_dir_all(root).expect("remove git interoperability fixture");
    }

    #[test]
    fn rejects_bad_pack_checksum() {
        let mut pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"hello\n");
        let last = pack.len() - 1;
        pack[last] ^= 1;
        assert!(PackFile::parse_sha1(&pack).is_err());
    }

    #[test]
    fn raw_pack_index_rejects_bad_pack_checksum() {
        let mut pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"hello\n");
        let last = pack.len() - 1;
        pack[last] ^= 1;
        assert!(PackIndex::write_v2_for_pack(&pack, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn pack_index_writer_preserves_duplicate_object_ids() {
        let oid = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"same\n")
            .expect("test operation should succeed");
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let entries = vec![
            PackIndexEntry {
                oid,
                crc32: 1,
                offset: 12,
            },
            PackIndexEntry {
                oid,
                crc32: 2,
                offset: 24,
            },
        ];
        let index = PackIndex::write_v2(ObjectFormat::Sha1, &entries, &pack_checksum)
            .expect("duplicate objects are valid in a pack index");
        let parsed = PackIndex::parse(&index, ObjectFormat::Sha1)
            .expect("duplicate-object pack index should parse");
        assert_eq!(parsed.entries.len(), 2);
        assert!(parsed.entries.iter().all(|entry| entry.oid == oid));
        assert!(parsed.find(&oid).is_some());
    }

    #[test]
    fn parses_single_entry_pack_index() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let index = single_entry_index(
            ObjectFormat::Sha1,
            oid,
            0x1234_5678,
            12,
            pack_checksum.clone(),
        );
        let parsed = PackIndex::parse_v2_sha1(&index).expect("test operation should succeed");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(
            parsed
                .find(&oid)
                .expect("test operation should succeed")
                .offset,
            12
        );
        assert_eq!(
            parsed
                .find(&oid)
                .expect("test operation should succeed")
                .crc32,
            0x1234_5678
        );
        assert_pack_index_view_matches_owned(&index, ObjectFormat::Sha1);
    }

    #[test]
    fn parses_single_entry_pack_index_v1() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let index =
            single_entry_index_v1(ObjectFormat::Sha1, oid, 0x1234_5678, pack_checksum.clone());
        let parsed =
            PackIndex::parse(&index, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(
            parsed
                .find(&oid)
                .expect("test operation should succeed")
                .offset,
            0x1234_5678
        );
        assert_eq!(
            parsed
                .find(&oid)
                .expect("test operation should succeed")
                .crc32,
            0
        );
        assert_pack_index_view_matches_owned(&index, ObjectFormat::Sha1);
    }

    #[test]
    fn rejects_bad_pack_index_v1_checksum() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let mut index = single_entry_index_v1(ObjectFormat::Sha1, oid, 12, pack_checksum);
        let last = index.len() - 1;
        index[last] ^= 1;
        assert!(PackIndex::parse(&index, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn pack_index_view_reads_v2_large_offsets() {
        let first = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"large offset a\n")
            .expect("test operation should succeed");
        let second =
            sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"large offset b\n")
                .expect("test operation should succeed");
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let entries = vec![
            PackIndexEntry {
                oid: first,
                crc32: 0x1111_2222,
                offset: 0x8000_0000,
            },
            PackIndexEntry {
                oid: second,
                crc32: 0x3333_4444,
                offset: 0x1_0000_0042,
            },
        ];
        let index = PackIndex::write_v2(ObjectFormat::Sha1, &entries, &pack_checksum)
            .expect("test operation should succeed");

        assert_pack_index_view_matches_owned(&index, ObjectFormat::Sha1);
        let view = PackIndexView::parse(&index, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        for entry in entries {
            assert_eq!(
                view.find(&entry.oid),
                Some(PackIndexLookup {
                    crc32: entry.crc32,
                    offset: entry.offset,
                })
            );
        }
    }

    #[test]
    fn pack_index_view_default_parse_checks_index_checksum() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let mut index = single_entry_index(ObjectFormat::Sha1, oid, 0x1234_5678, 12, pack_checksum);
        let last = index.len() - 1;
        index[last] ^= 1;

        assert!(PackIndexView::parse(&index, ObjectFormat::Sha1).is_err());
        let view = PackIndexView::parse_without_checksum(&index, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let trusted_view = PackIndexViewData::parse_trusted_without_checksum(
            Arc::from(index.clone().into_boxed_slice()),
            ObjectFormat::Sha1,
        )
        .expect("test operation should succeed");
        assert_eq!(
            view.find(&oid),
            Some(PackIndexLookup {
                crc32: 0x1234_5678,
                offset: 12,
            })
        );
        assert_eq!(
            trusted_view.find(&oid),
            Some(PackIndexLookup {
                crc32: 0x1234_5678,
                offset: 12,
            })
        );
    }

    #[test]
    fn reverse_index_resolves_oid_at_offset() {
        let objects = (0..3)
            .map(|idx| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("reverse index lookup object {idx}\n").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        let written = PackFile::write_packed(&objects, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let index = PackIndex::parse(&written.index, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let view = PackIndexViewData::parse_trusted_without_checksum(
            Arc::from(written.index.into_boxed_slice()),
            ObjectFormat::Sha1,
        )
        .expect("test operation should succeed");
        let positions = pack_order_index_positions(&index.entries);
        let reverse = PackReverseIndex::parse(
            &PackReverseIndex::write(ObjectFormat::Sha1, &positions, &index.pack_checksum)
                .expect("test operation should succeed"),
            ObjectFormat::Sha1,
            index.entries.len(),
        )
        .expect("test operation should succeed");

        for entry in &index.entries {
            assert_eq!(
                reverse
                    .oid_at_offset(&view, entry.offset)
                    .expect("test operation should succeed"),
                entry.oid
            );
        }
        assert!(reverse.oid_at_offset(&view, 999).is_none());
    }

    #[test]
    fn parses_pack_reverse_index() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let reverse_index = PackReverseIndex::write(ObjectFormat::Sha1, &[2, 0, 1], &pack_checksum)
            .expect("test operation should succeed");
        let parsed = PackReverseIndex::parse(&reverse_index, ObjectFormat::Sha1, 3)
            .expect("test operation should succeed");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.positions, vec![2, 0, 1]);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(
            PackReverseIndex::write(ObjectFormat::Sha1, &parsed.positions, &parsed.pack_checksum)
                .expect("test operation should succeed"),
            reverse_index
        );
    }

    #[test]
    fn rejects_bad_pack_reverse_index_checksum() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let mut reverse_index = PackReverseIndex::write(ObjectFormat::Sha1, &[0], &pack_checksum)
            .expect("test operation should succeed");
        let last = reverse_index.len() - 1;
        reverse_index[last] ^= 1;
        assert!(matches!(
            PackReverseIndex::parse(&reverse_index, ObjectFormat::Sha1, 1),
            Err(GitError::InvalidFormat(message)) if message == "invalid checksum"
        ));
    }

    #[test]
    fn classifies_pack_reverse_index_corruption_for_fsck() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let valid = PackReverseIndex::write(ObjectFormat::Sha1, &[0], &pack_checksum)
            .expect("test operation should succeed");
        for (offset, value, expected) in [
            (1, 7, "unknown signature"),
            (7, 2, "unsupported version 2"),
            (11, 3, "unsupported hash id 3"),
            (14, 7, "invalid rev-index position 1792"),
        ] {
            let mut corrupt = valid.clone();
            corrupt[offset] = value;
            assert!(matches!(
                PackReverseIndex::parse(&corrupt, ObjectFormat::Sha1, 1),
                Err(GitError::InvalidFormat(message)) if message == expected
            ));
        }
    }

    #[test]
    fn rejects_bad_pack_reverse_index_positions() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let duplicate = pack_reverse_index(ObjectFormat::Sha1, &[0, 0], pack_checksum.clone());
        assert!(PackReverseIndex::parse(&duplicate, ObjectFormat::Sha1, 2).is_err());
        let out_of_range = pack_reverse_index(ObjectFormat::Sha1, &[0, 2], pack_checksum);
        assert!(PackReverseIndex::parse(&out_of_range, ObjectFormat::Sha1, 2).is_err());
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        assert!(PackReverseIndex::write(ObjectFormat::Sha1, &[0, 0], &pack_checksum).is_err());
        assert!(PackReverseIndex::write(ObjectFormat::Sha1, &[0, 2], &pack_checksum).is_err());
    }

    #[test]
    fn parses_pack_mtimes() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let mtimes = PackMtimes::write(
            ObjectFormat::Sha1,
            &[1, 1_700_000_000, u32::MAX],
            &pack_checksum,
        )
        .expect("test operation should succeed");
        let parsed = PackMtimes::parse(&mtimes, ObjectFormat::Sha1, 3)
            .expect("test operation should succeed");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.mtimes, vec![1, 1_700_000_000, u32::MAX]);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(
            PackMtimes::write(ObjectFormat::Sha1, &parsed.mtimes, &parsed.pack_checksum)
                .expect("test operation should succeed"),
            mtimes
        );
    }

    #[test]
    fn rejects_bad_pack_mtimes_checksum() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let mut mtimes = PackMtimes::write(ObjectFormat::Sha1, &[1], &pack_checksum)
            .expect("test operation should succeed");
        let last = mtimes.len() - 1;
        mtimes[last] ^= 1;
        assert!(PackMtimes::parse(&mtimes, ObjectFormat::Sha1, 1).is_err());
    }

    #[test]
    fn rejects_bad_pack_mtimes_shape() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let mtimes = pack_mtimes(ObjectFormat::Sha1, &[1, 2], pack_checksum.clone());
        assert!(PackMtimes::parse(&mtimes, ObjectFormat::Sha1, 1).is_err());

        let mut wrong_hash = pack_mtimes(ObjectFormat::Sha1, &[1], pack_checksum);
        wrong_hash[11] = 2;
        let checksum_offset = wrong_hash.len() - ObjectFormat::Sha1.raw_len();
        let checksum = sley_core::digest_bytes(ObjectFormat::Sha1, &wrong_hash[..checksum_offset])
            .expect("test operation should succeed");
        wrong_hash[checksum_offset..].copy_from_slice(checksum.as_bytes());
        assert!(PackMtimes::parse(&wrong_hash, ObjectFormat::Sha1, 1).is_err());
    }

    #[test]
    fn parses_multi_pack_index_header_and_chunk_lookup() {
        let first = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"first object\n")
            .expect("test operation should succeed");
        let second = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"second object\n")
            .expect("test operation should succeed");
        let chunks = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0pack-b.idx\0\0\0".to_vec(),
            &[(first.clone(), 0, 12), (second.clone(), 1, 0x1_0000_0000)],
        );
        let midx = multi_pack_index(ObjectFormat::Sha1, 2, 2, &chunks);
        let parsed = MultiPackIndex::parse(&midx, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.pack_count, 2);
        assert_eq!(parsed.pack_names, vec!["pack-a.idx", "pack-b.idx"]);
        assert_eq!(parsed.object_count, 2);
        assert_eq!(parsed.objects.len(), 2);
        assert_eq!(
            parsed
                .find(&first)
                .expect("test operation should succeed")
                .pack_int_id,
            0
        );
        assert_eq!(
            parsed
                .find(&first)
                .expect("test operation should succeed")
                .offset,
            12
        );
        assert_eq!(
            parsed
                .find(&second)
                .expect("test operation should succeed")
                .pack_int_id,
            1
        );
        assert_eq!(
            parsed
                .find(&second)
                .expect("test operation should succeed")
                .offset,
            0x1_0000_0000
        );
        assert_eq!(parsed.reverse_index, None);
        assert_eq!(parsed.bitmapped_packs, None);
        assert_eq!(parsed.chunks.len(), 5);
        assert_eq!(parsed.chunks[0].id, *b"PNAM");
        assert_eq!(parsed.chunks[0].offset, 84);
        assert_eq!(parsed.chunks[0].len, 24);
        assert_eq!(parsed.chunks[1].id, *b"OIDF");
        assert_eq!(parsed.chunks[1].offset, 108);
        assert_eq!(parsed.chunks[1].len, 1024);
    }

    #[test]
    fn raw_multi_pack_index_lookup_finds_pack_and_offset() {
        let first = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"first object\n")
            .expect("test operation should succeed");
        let second = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"second object\n")
            .expect("test operation should succeed");
        let missing = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"missing\n")
            .expect("test operation should succeed");
        let chunks = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0pack-b.idx\0\0\0".to_vec(),
            &[(first.clone(), 0, 12), (second.clone(), 1, 0x1_0000_0000)],
        );
        let midx = Arc::new(multi_pack_index(ObjectFormat::Sha1, 2, 2, &chunks));
        let lookup = MultiPackIndexOidLookup::parse(midx, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        assert!(lookup.contains(&first));
        assert!(lookup.contains(&second));
        assert!(!lookup.contains(&missing));

        let first_entry = lookup
            .find(&first)
            .expect("test operation should succeed")
            .expect("object should be present");
        assert_eq!(
            lookup.pack_name(first_entry.pack_int_id),
            Some("pack-a.idx")
        );
        assert_eq!(first_entry.offset, 12);

        let second_entry = lookup
            .find(&second)
            .expect("test operation should succeed")
            .expect("object should be present");
        assert_eq!(
            lookup.pack_name(second_entry.pack_int_id),
            Some("pack-b.idx")
        );
        assert_eq!(second_entry.offset, 0x1_0000_0000);
        assert!(
            lookup
                .find(&missing)
                .expect("test operation should succeed")
                .is_none()
        );
    }

    #[test]
    fn rejects_bad_multi_pack_index_checksum() {
        let chunks = midx_chunks_with_pack_names(ObjectFormat::Sha1, Vec::new(), &[]);
        let mut midx = multi_pack_index(ObjectFormat::Sha1, 1, 0, &chunks);
        let last = midx.len() - 1;
        midx[last] ^= 1;
        assert!(MultiPackIndex::parse(&midx, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn rejects_bad_multi_pack_index_shape() {
        let chunks = midx_chunks_with_pack_names(ObjectFormat::Sha1, Vec::new(), &[]);
        let mut wrong_hash = multi_pack_index(ObjectFormat::Sha1, 1, 0, &chunks);
        wrong_hash[5] = 2;
        let checksum_offset = wrong_hash.len() - ObjectFormat::Sha1.raw_len();
        let checksum = sley_core::digest_bytes(ObjectFormat::Sha1, &wrong_hash[..checksum_offset])
            .expect("test operation should succeed");
        wrong_hash[checksum_offset..].copy_from_slice(checksum.as_bytes());
        assert!(MultiPackIndex::parse(&wrong_hash, ObjectFormat::Sha1).is_err());

        let mut missing_terminator = multi_pack_index(ObjectFormat::Sha1, 1, 0, &chunks);
        missing_terminator[12] = b'B';
        let checksum_offset = missing_terminator.len() - ObjectFormat::Sha1.raw_len();
        let checksum =
            sley_core::digest_bytes(ObjectFormat::Sha1, &missing_terminator[..checksum_offset])
                .expect("test operation should succeed");
        missing_terminator[checksum_offset..].copy_from_slice(checksum.as_bytes());
        assert!(MultiPackIndex::parse(&missing_terminator, ObjectFormat::Sha1).is_err());

        let mut bad_offset = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            0,
            &midx_chunks_with_pack_names(ObjectFormat::Sha1, Vec::new(), &[]),
        );
        bad_offset[16..24].copy_from_slice(&0u64.to_be_bytes());
        let checksum_offset = bad_offset.len() - ObjectFormat::Sha1.raw_len();
        let checksum = sley_core::digest_bytes(ObjectFormat::Sha1, &bad_offset[..checksum_offset])
            .expect("test operation should succeed");
        bad_offset[checksum_offset..].copy_from_slice(checksum.as_bytes());
        assert!(MultiPackIndex::parse(&bad_offset, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn rejects_bad_multi_pack_index_pack_names() {
        let missing = multi_pack_index(ObjectFormat::Sha1, 2, 1, &[]);
        assert!(MultiPackIndex::parse(&missing, ObjectFormat::Sha1).is_err());

        let too_few = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            2,
            &midx_chunks_with_pack_names(ObjectFormat::Sha1, b"pack-a.idx\0".to_vec(), &[]),
        );
        assert!(MultiPackIndex::parse(&too_few, ObjectFormat::Sha1).is_err());

        let bad_padding = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            1,
            &midx_chunks_with_pack_names(ObjectFormat::Sha1, b"pack-a.idx\0xxxx".to_vec(), &[]),
        );
        assert!(MultiPackIndex::parse(&bad_padding, ObjectFormat::Sha1).is_err());

        let unsorted_v1 = multi_pack_index(
            ObjectFormat::Sha1,
            1,
            2,
            &midx_chunks_with_pack_names(
                ObjectFormat::Sha1,
                b"pack-b.idx\0pack-a.idx\0".to_vec(),
                &[],
            ),
        );
        assert!(MultiPackIndex::parse(&unsorted_v1, ObjectFormat::Sha1).is_err());

        let unsorted_v2 = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            2,
            &midx_chunks_with_pack_names(
                ObjectFormat::Sha1,
                b"pack-b.idx\0pack-a.idx\0".to_vec(),
                &[],
            ),
        );
        let parsed = MultiPackIndex::parse(&unsorted_v2, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(parsed.pack_names, vec!["pack-b.idx", "pack-a.idx"]);
    }

    #[test]
    fn rejects_bad_multi_pack_index_object_tables() {
        let oid_a = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let oid_b = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");

        let missing_oidf = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            1,
            &[(*b"PNAM", b"pack-a.idx\0\0".to_vec())],
        );
        assert!(MultiPackIndex::parse(&missing_oidf, ObjectFormat::Sha1).is_err());

        let bad_fanout = vec![
            (*b"PNAM", b"pack-a.idx\0\0".to_vec()),
            (*b"OIDF", vec![0; 256 * 4]),
            (*b"OIDL", oid_a.as_bytes().to_vec()),
            (*b"OOFF", midx_ooff_entries(&[(0, 12)], &mut Vec::new())),
        ];
        let bad_fanout = multi_pack_index(ObjectFormat::Sha1, 2, 1, &bad_fanout);
        assert!(MultiPackIndex::parse(&bad_fanout, ObjectFormat::Sha1).is_err());

        let mut unsorted = Vec::new();
        unsorted.push((*b"PNAM", b"pack-a.idx\0\0".to_vec()));
        unsorted.push((*b"OIDF", midx_oid_fanout(&[oid_a.clone(), oid_b.clone()])));
        let mut oid_lookup = Vec::new();
        oid_lookup.extend_from_slice(oid_b.as_bytes());
        oid_lookup.extend_from_slice(oid_a.as_bytes());
        unsorted.push((*b"OIDL", oid_lookup));
        unsorted.push((
            *b"OOFF",
            midx_ooff_entries(&[(0, 12), (0, 24)], &mut Vec::new()),
        ));
        let unsorted = multi_pack_index(ObjectFormat::Sha1, 2, 1, &unsorted);
        assert!(MultiPackIndex::parse(&unsorted, ObjectFormat::Sha1).is_err());

        let bad_pack = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            1,
            &midx_chunks_with_pack_names(
                ObjectFormat::Sha1,
                b"pack-a.idx\0\0".to_vec(),
                &[(oid_a.clone(), 1, 12)],
            ),
        );
        assert!(MultiPackIndex::parse(&bad_pack, ObjectFormat::Sha1).is_err());

        let mut large_offsets = Vec::new();
        let missing_loff = vec![
            (*b"PNAM", b"pack-a.idx\0\0".to_vec()),
            (*b"OIDF", midx_oid_fanout(std::slice::from_ref(&oid_a))),
            (*b"OIDL", oid_a.as_bytes().to_vec()),
            (
                *b"OOFF",
                midx_ooff_entries(&[(0, 0x1_0000_0000)], &mut large_offsets),
            ),
        ];
        let missing_loff = multi_pack_index(ObjectFormat::Sha1, 2, 1, &missing_loff);
        assert!(MultiPackIndex::parse(&missing_loff, ObjectFormat::Sha1).is_err());

        let mut bad_loff =
            midx_chunks_with_pack_names(ObjectFormat::Sha1, b"pack-a.idx\0\0".to_vec(), &[]);
        bad_loff.push((*b"LOFF", vec![0]));
        let bad_loff = multi_pack_index(ObjectFormat::Sha1, 2, 1, &bad_loff);
        assert!(MultiPackIndex::parse(&bad_loff, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn parses_multi_pack_index_bitmap_chunks() {
        let first = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"first object\n")
            .expect("test operation should succeed");
        let second = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"second object\n")
            .expect("test operation should succeed");
        let mut chunks = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0pack-b.idx\0\0\0".to_vec(),
            &[(first, 0, 12), (second, 1, 24)],
        );
        chunks.push((*b"RIDX", midx_u32_table(&[1, 0])));
        chunks.push((*b"BTMP", midx_bitmap_packs(&[(0, 1), (1, 1)])));
        let midx = multi_pack_index(ObjectFormat::Sha1, 2, 2, &chunks);

        let parsed = MultiPackIndex::parse(&midx, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(parsed.reverse_index, Some(vec![1, 0]));
        assert_eq!(
            parsed.bitmapped_packs,
            Some(vec![
                MultiPackBitmapPack {
                    bitmap_pos: 0,
                    bitmap_nr: 1,
                },
                MultiPackBitmapPack {
                    bitmap_pos: 1,
                    bitmap_nr: 1,
                },
            ])
        );
    }

    #[test]
    fn writes_multi_pack_index_that_round_trips() {
        let first = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"first object\n")
            .expect("test operation should succeed");
        let second = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"second object\n")
            .expect("test operation should succeed");
        let bytes = MultiPackIndex::write(
            ObjectFormat::Sha1,
            2,
            &["pack-b.idx".into(), "pack-a.idx".into()],
            &[
                MultiPackIndexEntry {
                    oid: second.clone(),
                    pack_int_id: 0,
                    offset: 0x1_0000_0000,
                    force_large_offset: false,
                },
                MultiPackIndexEntry {
                    oid: first.clone(),
                    pack_int_id: 1,
                    offset: 12,
                    force_large_offset: false,
                },
            ],
        )
        .expect("test operation should succeed");

        let parsed = MultiPackIndex::parse(&bytes, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.pack_names, vec!["pack-b.idx", "pack-a.idx"]);
        assert_eq!(parsed.object_count, 2);
        assert_eq!(
            parsed
                .find(&first)
                .expect("test operation should succeed")
                .pack_int_id,
            1
        );
        assert_eq!(
            parsed
                .find(&first)
                .expect("test operation should succeed")
                .offset,
            12
        );
        assert_eq!(
            parsed
                .find(&second)
                .expect("test operation should succeed")
                .pack_int_id,
            0
        );
        assert_eq!(
            parsed
                .find(&second)
                .expect("test operation should succeed")
                .offset,
            0x1_0000_0000
        );
        assert!(parsed.chunks.iter().any(|chunk| chunk.id == *b"LOFF"));
    }

    #[test]
    fn write_multi_pack_index_rejects_invalid_inputs() {
        let oid = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"object\n")
            .expect("test operation should succeed");
        assert!(MultiPackIndex::write(ObjectFormat::Sha1, 3, &["pack-a.idx".into()], &[]).is_err());
        assert!(
            MultiPackIndex::write(
                ObjectFormat::Sha1,
                1,
                &["pack-b.idx".into(), "pack-a.idx".into()],
                &[],
            )
            .is_err()
        );
        assert!(MultiPackIndex::write(ObjectFormat::Sha1, 2, &["pack/a.idx".into()], &[]).is_err());
        assert!(
            MultiPackIndex::write(
                ObjectFormat::Sha1,
                2,
                &["pack-a.idx".into()],
                &[MultiPackIndexEntry {
                    oid,
                    pack_int_id: 1,
                    offset: 12,
                    force_large_offset: false,
                }],
            )
            .is_err()
        );
        assert!(
            MultiPackIndex::write(
                ObjectFormat::Sha1,
                2,
                &["pack-a.idx".into()],
                &[
                    MultiPackIndexEntry {
                        oid,
                        pack_int_id: 0,
                        offset: 12,
                        force_large_offset: false,
                    },
                    MultiPackIndexEntry {
                        oid,
                        pack_int_id: 0,
                        offset: 24,
                        force_large_offset: false,
                    },
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_bad_multi_pack_index_bitmap_chunks() {
        let oid_a = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let oid_b = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");

        let mut duplicate_ridx = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0\0".to_vec(),
            &[(oid_a.clone(), 0, 12), (oid_b.clone(), 0, 24)],
        );
        duplicate_ridx.push((*b"RIDX", midx_u32_table(&[0, 0])));
        let duplicate_ridx = multi_pack_index(ObjectFormat::Sha1, 2, 1, &duplicate_ridx);
        assert!(MultiPackIndex::parse(&duplicate_ridx, ObjectFormat::Sha1).is_err());

        let mut short_btmp = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0pack-b.idx\0\0\0".to_vec(),
            &[(oid_a.clone(), 0, 12), (oid_b.clone(), 1, 24)],
        );
        short_btmp.push((*b"BTMP", midx_bitmap_packs(&[(0, 1)])));
        let short_btmp = multi_pack_index(ObjectFormat::Sha1, 2, 2, &short_btmp);
        assert!(MultiPackIndex::parse(&short_btmp, ObjectFormat::Sha1).is_err());

        let mut out_of_range_btmp = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0\0".to_vec(),
            &[(oid_a, 0, 12), (oid_b, 0, 24)],
        );
        out_of_range_btmp.push((*b"BTMP", midx_bitmap_packs(&[(1, 2)])));
        let out_of_range_btmp = multi_pack_index(ObjectFormat::Sha1, 2, 1, &out_of_range_btmp);
        assert!(MultiPackIndex::parse(&out_of_range_btmp, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn parses_pack_bitmap_index_with_hash_cache() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let bitmap = pack_bitmap_index(
            ObjectFormat::Sha1,
            3,
            PackBitmapIndex::OPTION_FULL_DAG | PackBitmapIndex::OPTION_HASH_CACHE,
            &pack_checksum,
            &[(2, 0, 1, &[0b101])],
            Some(&[0x1111_1111, 0x2222_2222, 0x3333_3333]),
        );

        let parsed = PackBitmapIndex::parse(&bitmap, ObjectFormat::Sha1, 3)
            .expect("test operation should succeed");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(
            parsed.options,
            PackBitmapIndex::OPTION_FULL_DAG | PackBitmapIndex::OPTION_HASH_CACHE
        );
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.type_bitmaps.commits.bit_size, 3);
        assert_eq!(parsed.type_bitmaps.trees.bit_size, 3);
        assert_eq!(parsed.entries.len(), 1);
        let entry = parsed
            .entry_for_index_position(2)
            .expect("test operation should succeed");
        assert_eq!(entry.xor_offset, 0);
        assert_eq!(entry.flags, 1);
        assert_eq!(entry.bitmap.words, ewah_literal_words(&[0b101]));
        assert_eq!(
            parsed.name_hash_cache,
            Some(vec![0x1111_1111, 0x2222_2222, 0x3333_3333])
        );
    }

    #[test]
    fn parses_pack_bitmap_index_sha256() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha256, b"pack")
            .expect("test operation should succeed");
        let bitmap = pack_bitmap_index(
            ObjectFormat::Sha256,
            2,
            PackBitmapIndex::OPTION_FULL_DAG,
            &pack_checksum,
            &[(0, 0, 0, &[0b11])],
            None,
        );

        let parsed = PackBitmapIndex::parse(&bitmap, ObjectFormat::Sha256, 2)
            .expect("test operation should succeed");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha256);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.index_checksum.format(), ObjectFormat::Sha256);
        assert_eq!(parsed.entries[0].object_position, 0);
        assert_eq!(parsed.name_hash_cache, None);
    }

    #[test]
    fn parses_upstream_git_written_pack_bitmap_index() {
        let root = unique_temp_dir("git-pack-bitmap-upstream");
        fs::create_dir_all(&root).expect("test operation should succeed");
        {
            run_git_success(&root, &["init", "-q", "-b", "main"]);
            run_git_success(
                &root,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-q",
                    "-m",
                    "one",
                ],
            );
            run_git_success(
                &root,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-q",
                    "-m",
                    "two",
                ],
            );
            run_git_success(&root, &["repack", "-adb"]);
            let pack_dir = root.join(".git").join("objects").join("pack");
            let idx_path = single_path_with_extension(&pack_dir, "idx");
            let bitmap_path = single_path_with_extension(&pack_dir, "bitmap");
            let index = PackIndex::parse(
                &fs::read(idx_path).expect("test operation should succeed"),
                ObjectFormat::Sha1,
            )
            .expect("test operation should succeed");
            let bitmap = PackBitmapIndex::parse(
                &fs::read(bitmap_path).expect("test operation should succeed"),
                ObjectFormat::Sha1,
                index.entries.len(),
            )
            .expect("test operation should succeed");
            assert_eq!(bitmap.pack_checksum, index.pack_checksum);
            assert!(!bitmap.entries.is_empty());
        };
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_bad_pack_bitmap_index_header_and_checksum() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let bitmap = pack_bitmap_index(
            ObjectFormat::Sha1,
            1,
            PackBitmapIndex::OPTION_FULL_DAG,
            &pack_checksum,
            &[(0, 0, 0, &[1])],
            None,
        );

        let mut bad_signature = bitmap.clone();
        bad_signature[0] = b'X';
        assert!(PackBitmapIndex::parse(&bad_signature, ObjectFormat::Sha1, 1).is_err());

        let mut bad_version = bitmap.clone();
        bad_version[5] = 2;
        refresh_trailing_checksum(ObjectFormat::Sha1, &mut bad_version);
        assert!(PackBitmapIndex::parse(&bad_version, ObjectFormat::Sha1, 1).is_err());

        let mut bad_option = bitmap.clone();
        bad_option[7] = 0x20;
        refresh_trailing_checksum(ObjectFormat::Sha1, &mut bad_option);
        assert!(PackBitmapIndex::parse(&bad_option, ObjectFormat::Sha1, 1).is_err());

        let mut bad_checksum = bitmap;
        let last = bad_checksum.len() - 1;
        bad_checksum[last] ^= 1;
        assert!(PackBitmapIndex::parse(&bad_checksum, ObjectFormat::Sha1, 1).is_err());
    }

    #[test]
    fn rejects_bad_pack_bitmap_index_ewah_and_entries() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha1, b"pack")
            .expect("test operation should succeed");
        let bitmap = pack_bitmap_index(
            ObjectFormat::Sha1,
            2,
            PackBitmapIndex::OPTION_FULL_DAG,
            &pack_checksum,
            &[(0, 0, 0, &[0b01]), (1, 1, 0, &[0b11])],
            None,
        );

        let mut truncated = bitmap;
        truncated.truncate(truncated.len() - ObjectFormat::Sha1.raw_len() - 1);
        refresh_trailing_checksum(ObjectFormat::Sha1, &mut truncated);
        assert!(PackBitmapIndex::parse(&truncated, ObjectFormat::Sha1, 2).is_err());

        let mut out_of_range_position = pack_bitmap_index(
            ObjectFormat::Sha1,
            2,
            PackBitmapIndex::OPTION_FULL_DAG,
            &pack_checksum,
            &[(2, 0, 0, &[0b01])],
            None,
        );
        assert!(PackBitmapIndex::parse(&out_of_range_position, ObjectFormat::Sha1, 2).is_err());
        refresh_trailing_checksum(ObjectFormat::Sha1, &mut out_of_range_position);
        assert!(PackBitmapIndex::parse(&out_of_range_position, ObjectFormat::Sha1, 2).is_err());

        let invalid_xor = pack_bitmap_index(
            ObjectFormat::Sha1,
            2,
            PackBitmapIndex::OPTION_FULL_DAG,
            &pack_checksum,
            &[(0, 1, 0, &[0b01])],
            None,
        );
        assert!(PackBitmapIndex::parse(&invalid_xor, ObjectFormat::Sha1, 2).is_err());
    }

    #[test]
    fn parses_single_entry_pack_index_sha256() {
        let oid = sley_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"hello sha256\n")
            .expect("test operation should succeed");
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha256, b"pack")
            .expect("test operation should succeed");
        let index = single_entry_index(
            ObjectFormat::Sha256,
            oid,
            0x1234_5678,
            12,
            pack_checksum.clone(),
        );
        let parsed =
            PackIndex::parse(&index, ObjectFormat::Sha256).expect("test operation should succeed");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(
            parsed
                .find(&oid)
                .expect("test operation should succeed")
                .offset,
            12
        );
        assert_eq!(
            parsed
                .find(&oid)
                .expect("test operation should succeed")
                .crc32,
            0x1234_5678
        );
        assert_eq!(parsed.index_checksum.format(), ObjectFormat::Sha256);
        assert_pack_index_view_matches_owned(&index, ObjectFormat::Sha256);
    }

    #[test]
    fn write_packed_deltifies_similar_blobs_and_round_trips_sha1() {
        write_packed_deltifies_similar_blobs_and_round_trips(ObjectFormat::Sha1);
    }

    #[test]
    fn write_packed_deltifies_similar_blobs_and_round_trips_sha256() {
        write_packed_deltifies_similar_blobs_and_round_trips(ObjectFormat::Sha256);
    }

    #[test]
    fn write_packed_rejects_duplicate_objects() {
        let object = EncodedObject::new(ObjectType::Blob, b"same\n".to_vec());
        assert!(PackFile::write_packed(&[object.clone(), object], ObjectFormat::Sha1,).is_err());
    }

    #[test]
    fn write_packed_with_known_ids_validates_ids_before_trusting_them() {
        let object = EncodedObject::new(ObjectType::Blob, b"same\n".to_vec());
        let sha1 = object
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let sha256 = object
            .object_id(ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let duplicate = [
            PackInput {
                oid: &sha1,
                object: &object,
            },
            PackInput {
                oid: &sha1,
                object: &object,
            },
        ];
        assert!(PackFile::write_packed_with_known_ids(&duplicate, ObjectFormat::Sha1).is_err());

        let wrong_format = [PackInput {
            oid: &sha256,
            object: &object,
        }];
        assert!(PackFile::write_packed_with_known_ids(&wrong_format, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn write_packed_with_known_ids_to_writer_matches_in_memory_pack() {
        let objects = similar_blob_family(6);
        let object_ids = objects
            .iter()
            .map(|object| {
                object
                    .object_id(ObjectFormat::Sha1)
                    .expect("test operation should succeed")
            })
            .collect::<Vec<_>>();
        let inputs = objects
            .iter()
            .zip(&object_ids)
            .map(|(object, oid)| PackInput { oid, object })
            .collect::<Vec<_>>();
        let options = PackWriteOptions::new();
        let in_memory = PackFile::write_packed_with_known_ids_and_options(
            &inputs,
            ObjectFormat::Sha1,
            &options,
        )
        .expect("test operation should succeed");
        let mut written = Vec::new();
        let streamed = PackFile::write_packed_with_known_ids_to_writer(
            &inputs,
            ObjectFormat::Sha1,
            &options,
            &mut written,
        )
        .expect("test operation should succeed");

        assert_eq!(written, in_memory.pack);
        assert_eq!(streamed.index, in_memory.index);
        assert_eq!(streamed.checksum, in_memory.checksum);
        assert_eq!(streamed.entries, in_memory.entries);
        assert_eq!(streamed.delta_count, in_memory.delta_count);
        assert_eq!(streamed.pack_size, in_memory.pack.len() as u64);
    }

    #[test]
    fn write_packed_from_source_to_writer_deltifies_across_windows() {
        let format = ObjectFormat::Sha1;
        // Keep this candidate inside Git's first-delta budget (half the
        // target size minus one raw object id), while still straddling two
        // byte-budgeted compression windows.
        let shared = b"cross-window base payload with enough shared anchors\n".repeat(64);
        let mut base_body = shared.clone();
        base_body.extend_from_slice(b"base\n");
        let mut target_body = shared;
        target_body.extend_from_slice(b"target\n");
        let objects = vec![
            EncodedObject::new(ObjectType::Blob, base_body),
            EncodedObject::new(ObjectType::Blob, target_body),
        ];

        let object_ids = objects
            .iter()
            .map(|object| {
                object
                    .object_id(format)
                    .expect("test operation should succeed")
            })
            .collect::<Vec<_>>();
        let base_oid = object_ids[0];
        let target_oid = object_ids[1];
        let object_map = object_ids
            .iter()
            .copied()
            .zip(objects.into_iter().map(Arc::new))
            .collect::<HashMap<_, _>>();

        let options = PackWriteOptions::new().with_reorder(false).with_window(10);
        let one_object = object_map.get(&base_oid).expect("base").body.len() as u64 + 80;
        let limits =
            PackWriteLimits::new().with_compression_working_set(ByteBudget::new(one_object));
        let mut written = Vec::new();
        let summary = PackFile::write_packed_from_source_to_writer(
            object_ids.iter().copied(),
            2,
            format,
            &options,
            limits,
            |oid| {
                object_map
                    .get(oid)
                    .cloned()
                    .ok_or_else(|| GitError::not_found(format!("missing test object {oid}")))
            },
            &mut written,
        )
        .expect("test operation should succeed");

        assert!(
            summary.delta_count > 0,
            "expected source-backed streaming writer to find deltas"
        );
        let stats =
            PackFile::verify_pack_stats(&written, format).expect("test operation should succeed");
        let target = stats
            .objects
            .iter()
            .find(|entry| entry.oid == target_oid)
            .expect("target object should be present");
        assert_eq!(target.base_oid, Some(base_oid));
    }

    fn write_packed_deltifies_similar_blobs_and_round_trips(format: ObjectFormat) {
        let objects = similar_blob_family(8);
        let packed =
            PackFile::write_packed(&objects, format).expect("test operation should succeed");
        let undeltified =
            PackFile::write_undeltified(&objects, format).expect("test operation should succeed");

        // The whole point of delta selection: the packed output is smaller than
        // storing every object undeltified.
        assert!(
            packed.pack.len() < undeltified.pack.len(),
            "expected delta pack ({}) smaller than undeltified pack ({})",
            packed.pack.len(),
            undeltified.pack.len()
        );

        // At least one object must actually be stored as a delta.
        let kinds = pack_entry_kinds(&packed.pack, format);
        let delta_count = kinds
            .iter()
            .filter(|kind| matches!(kind, PackObjectKind::OfsDelta | PackObjectKind::RefDelta))
            .count();
        assert!(
            delta_count >= 1,
            "expected at least one delta entry, found kinds {kinds:?}"
        );

        // Round-trip: every original object reconstructs byte-for-byte.
        let parsed = PackFile::parse(&packed.pack, format).expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), objects.len());
        for object in &objects {
            let oid = object
                .object_id(format)
                .expect("test operation should succeed");
            let found = parsed
                .entries
                .iter()
                .find(|entry| entry.entry.oid == oid)
                .unwrap_or_else(|| panic!("object {oid} missing from parsed pack"));
            assert_eq!(&found.object, object, "object {oid} did not round-trip");
        }

        // The index must agree with the pack and locate every object.
        let index = PackIndex::parse(&packed.index, format).expect("test operation should succeed");
        assert_eq!(index.pack_checksum, packed.checksum);
        for object in &objects {
            let oid = object
                .object_id(format)
                .expect("test operation should succeed");
            assert!(index.find(&oid).is_some(), "index missing {oid}");
        }
    }

    #[test]
    fn write_packed_emits_ofs_delta_by_default() {
        let objects = similar_blob_family(6);
        let packed = PackFile::write_packed(&objects, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let kinds = pack_entry_kinds(&packed.pack, ObjectFormat::Sha1);
        assert!(
            kinds.contains(&PackObjectKind::OfsDelta),
            "expected an ofs-delta entry by default, found {kinds:?}"
        );
        assert!(
            !kinds.contains(&PackObjectKind::RefDelta),
            "default self-contained pack must not use ref-delta, found {kinds:?}"
        );
        // Round-trips.
        assert!(PackFile::parse(&packed.pack, ObjectFormat::Sha1).is_ok());
    }

    #[test]
    fn write_packed_can_emit_ref_delta() {
        let objects = similar_blob_family(6);
        let options = PackWriteOptions::new().with_prefer_ofs_delta(false);
        let packed = PackFile::write_packed_with_options(&objects, ObjectFormat::Sha1, &options)
            .expect("test operation should succeed");
        let kinds = pack_entry_kinds(&packed.pack, ObjectFormat::Sha1);
        assert!(
            kinds.contains(&PackObjectKind::RefDelta),
            "expected a ref-delta entry, found {kinds:?}"
        );
        assert!(
            !kinds.contains(&PackObjectKind::OfsDelta),
            "ref-delta mode must not emit ofs-delta, found {kinds:?}"
        );

        // Ref-delta packs are still self-contained here, so they round-trip
        // without any external base lookup.
        let parsed = PackFile::parse(&packed.pack, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), objects.len());
    }

    #[test]
    fn write_packed_bounds_delta_chain_depth() {
        // A long chain of progressively-modified blobs. With a large window
        // every object could otherwise delta against its immediate predecessor,
        // forming a chain as long as the input.
        let objects = incremental_blob_chain(20);
        let format = ObjectFormat::Sha1;

        for max_depth in [1usize, 2, 5] {
            let options = PackWriteOptions::new()
                .with_window(20)
                .with_depth(max_depth);
            let packed = PackFile::write_packed_with_options(&objects, format, &options)
                .expect("test operation should succeed");

            let depths = pack_entry_depths(&packed.pack, format);
            let observed = depths.iter().copied().max().unwrap_or(0);
            assert!(
                observed <= max_depth,
                "max chain depth {observed} exceeded bound {max_depth}"
            );

            // Still correct: round-trips byte-for-byte.
            let parsed =
                PackFile::parse(&packed.pack, format).expect("test operation should succeed");
            for object in &objects {
                let oid = object
                    .object_id(format)
                    .expect("test operation should succeed");
                let found = parsed
                    .entries
                    .iter()
                    .find(|entry| entry.entry.oid == oid)
                    .expect("test operation should succeed");
                assert_eq!(&found.object, object);
            }
        }
    }

    #[test]
    fn write_packed_depth_zero_stores_everything_undeltified() {
        let objects = similar_blob_family(5);
        let options = PackWriteOptions::new().with_depth(0);
        let packed = PackFile::write_packed_with_options(&objects, ObjectFormat::Sha1, &options)
            .expect("test operation should succeed");
        let kinds = pack_entry_kinds(&packed.pack, ObjectFormat::Sha1);
        assert!(
            kinds
                .iter()
                .all(|kind| !matches!(kind, PackObjectKind::OfsDelta | PackObjectKind::RefDelta)),
            "depth 0 must disable deltas, found {kinds:?}"
        );
    }

    #[test]
    fn write_thin_uses_external_base_and_round_trips_sha1() {
        write_thin_uses_external_base_and_round_trips(ObjectFormat::Sha1);
    }

    #[test]
    fn write_thin_uses_external_base_and_round_trips_sha256() {
        write_thin_uses_external_base_and_round_trips(ObjectFormat::Sha256);
    }

    fn write_thin_uses_external_base_and_round_trips(format: ObjectFormat) {
        // The base object stays OUT of the pack; only `target` is written, as a
        // ref-delta against the external base's object id.
        let base = blob_with_marker("EXTERNAL-BASE");
        let target = blob_with_marker("EXTERNAL-TARGET");
        let base_oid = base
            .object_id(format)
            .expect("test operation should succeed");

        let mut external = HashMap::new();
        external.insert(base_oid, base.clone());
        let packed = PackFile::write_thin(std::slice::from_ref(&target), format, external)
            .expect("test operation should succeed");

        // Exactly one entry, encoded as a ref-delta to the external base.
        let kinds = pack_entry_kinds(&packed.pack, format);
        assert_eq!(kinds, vec![PackObjectKind::RefDelta]);

        // The external base reference must be the base oid.
        let mut offset = 12usize;
        let header =
            parse_entry_header(&packed.pack, &mut offset).expect("test operation should succeed");
        assert_eq!(header.kind, PackObjectKind::RefDelta);
        let referenced =
            ObjectId::from_raw(format, &packed.pack[offset..offset + format.raw_len()])
                .expect("test operation should succeed");
        assert_eq!(referenced, base_oid);

        // A plain (non-thin) parse fails: the base is not present.
        assert!(PackFile::parse(&packed.pack, format).is_err());

        // A thin parse that supplies the external base reconstructs the target.
        let parsed = PackFile::parse_thin(&packed.pack, format, |oid| {
            if oid == &base_oid {
                Ok(Some(base.clone()))
            } else {
                Ok(None)
            }
        })
        .expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].object, target);
    }

    #[test]
    fn write_packed_preserves_distinct_objects_with_no_similarity() {
        // Unrelated objects: nothing should delta, but the pack must still be
        // valid and complete.
        let objects = vec![
            EncodedObject::new(ObjectType::Blob, b"alpha distinct\n".to_vec()),
            EncodedObject::new(ObjectType::Tree, vec![0u8; 0]),
            EncodedObject::new(ObjectType::Commit, b"tree 0000\n".to_vec()),
        ];
        let format = ObjectFormat::Sha1;
        let packed =
            PackFile::write_packed(&objects, format).expect("test operation should succeed");
        let parsed = PackFile::parse(&packed.pack, format).expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), objects.len());
        for object in &objects {
            let oid = object
                .object_id(format)
                .expect("test operation should succeed");
            assert!(parsed.entries.iter().any(|entry| entry.entry.oid == oid));
        }
    }

    /// Build a family of blobs that all share a large common region but differ
    /// in a marker placed in the *middle*, so a good delta finds copy regions on
    /// both sides of the change.
    fn similar_blob_family(count: usize) -> Vec<EncodedObject> {
        let mut common_head = Vec::new();
        for _ in 0..200 {
            common_head.extend_from_slice(b"shared header line for delta testing\n");
        }
        let mut common_tail = Vec::new();
        for _ in 0..200 {
            common_tail.extend_from_slice(b"shared trailer line for delta testing\n");
        }
        (0..count)
            .map(|idx| {
                let mut body = common_head.clone();
                body.extend_from_slice(format!("UNIQUE MIDDLE MARKER NUMBER {idx}\n").as_bytes());
                body.extend_from_slice(&common_tail);
                EncodedObject::new(ObjectType::Blob, body)
            })
            .collect()
    }

    /// Build a chain where each blob is the previous one plus an appended line,
    /// so each is highly similar to its predecessor.
    fn incremental_blob_chain(count: usize) -> Vec<EncodedObject> {
        let mut body = Vec::new();
        for _ in 0..100 {
            body.extend_from_slice(b"baseline content shared across the whole chain\n");
        }
        let mut objects = Vec::with_capacity(count);
        for idx in 0..count {
            body.extend_from_slice(format!("appended unique line {idx}\n").as_bytes());
            objects.push(EncodedObject::new(ObjectType::Blob, body.clone()));
        }
        objects
    }

    fn blob_with_marker(marker: &str) -> EncodedObject {
        let mut body = Vec::new();
        for _ in 0..150 {
            body.extend_from_slice(b"common body shared between base and target\n");
        }
        body.extend_from_slice(marker.as_bytes());
        body.push(b'\n');
        for _ in 0..150 {
            body.extend_from_slice(b"more common body shared between objects\n");
        }
        EncodedObject::new(ObjectType::Blob, body)
    }

    /// Classify every entry in a pack (in pack order) by its on-disk kind.
    fn pack_entry_kinds(pack: &[u8], format: ObjectFormat) -> Vec<PackObjectKind> {
        pack_entry_descriptors(pack, format)
            .into_iter()
            .map(|descriptor| descriptor.kind)
            .collect()
    }

    /// Compute each entry's delta chain depth (0 = undeltified base), in pack
    /// order. Entries always appear after their in-pack bases, so a single
    /// forward pass suffices.
    fn pack_entry_depths(pack: &[u8], format: ObjectFormat) -> Vec<usize> {
        let descriptors = pack_entry_descriptors(pack, format);
        let mut depth_by_offset: HashMap<u64, usize> = HashMap::new();
        let mut depths = Vec::with_capacity(descriptors.len());
        for descriptor in &descriptors {
            let depth = match &descriptor.base {
                EntryBase::None => 0,
                EntryBase::Offset(base_offset) => {
                    depth_by_offset.get(base_offset).copied().unwrap_or(0) + 1
                }
                // Ref-delta to an in-pack base: look it up by offset via oid is
                // unnecessary for these tests (which only use ofs-delta for the
                // chains), so treat as depth 1 if unknown.
                EntryBase::Ref => 1,
            };
            depth_by_offset.insert(descriptor.offset, depth);
            depths.push(depth);
        }
        depths
    }

    struct EntryDescriptor {
        offset: u64,
        kind: PackObjectKind,
        base: EntryBase,
    }

    enum EntryBase {
        None,
        Offset(u64),
        Ref,
    }

    fn pack_entry_descriptors(pack: &[u8], format: ObjectFormat) -> Vec<EntryDescriptor> {
        let trailer_offset = pack.len() - format.raw_len();
        let count = u32_be(&pack[8..12]) as usize;
        let mut offset = 12usize;
        let mut descriptors = Vec::with_capacity(count);
        for _ in 0..count {
            let entry_offset = offset as u64;
            let header =
                parse_entry_header(pack, &mut offset).expect("test operation should succeed");
            let base = match header.kind {
                PackObjectKind::OfsDelta => {
                    let base_offset = parse_ofs_delta_base_offset(pack, &mut offset, entry_offset)
                        .expect("test operation should succeed");
                    EntryBase::Offset(base_offset)
                }
                PackObjectKind::RefDelta => {
                    offset += format.raw_len();
                    EntryBase::Ref
                }
                _ => EntryBase::None,
            };
            let mut decoder = ZlibDecoder::new(&pack[offset..trailer_offset]);
            let mut body = Vec::new();
            decoder
                .read_to_end(&mut body)
                .expect("test operation should succeed");
            offset += decoder.total_in() as usize;
            descriptors.push(EntryDescriptor {
                offset: entry_offset,
                kind: header.kind,
                base,
            });
        }
        descriptors
    }

    fn similar_blob_objects() -> (EncodedObject, EncodedObject) {
        let mut base = Vec::new();
        for _ in 0..300 {
            base.extend_from_slice(b"common payload\n");
        }
        base.extend_from_slice(b"base\n");
        let mut changed = Vec::new();
        for _ in 0..300 {
            changed.extend_from_slice(b"common payload\n");
        }
        changed.extend_from_slice(b"changed\n");
        (
            EncodedObject::new(ObjectType::Blob, base),
            EncodedObject::new(ObjectType::Blob, changed),
        )
    }

    fn single_object_pack(format: ObjectFormat, object_type: ObjectType, body: &[u8]) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        write_entry_header(&mut pack, object_type, body.len() as u64);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(body)
            .expect("test operation should succeed");
        pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));
        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    #[derive(Clone, Copy, Debug)]
    enum DeltaKind {
        Offset,
        Ref,
    }

    fn two_object_delta_pack(
        format: ObjectFormat,
        base: &[u8],
        result: &[u8],
        delta_kind: DeltaKind,
    ) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&2u32.to_be_bytes());

        let base_offset = pack.len();
        write_entry_header(&mut pack, ObjectType::Blob, base.len() as u64);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(base)
            .expect("test operation should succeed");
        pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));

        let delta = append_suffix_delta(base, result);
        let delta_offset = pack.len();
        write_pack_entry_header_kind(
            &mut pack,
            match delta_kind {
                DeltaKind::Offset => 6,
                DeltaKind::Ref => 7,
            },
            delta.len() as u64,
        );
        match delta_kind {
            DeltaKind::Offset => write_ofs_delta_offset(&mut pack, delta_offset - base_offset),
            DeltaKind::Ref => {
                let base_oid = sley_core::object_id_for_bytes(format, "blob", base)
                    .expect("test operation should succeed");
                pack.extend_from_slice(base_oid.as_bytes());
            }
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&delta)
            .expect("test operation should succeed");
        pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));

        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    fn thin_ref_delta_pack(format: ObjectFormat, base: &[u8], result: &[u8]) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());

        let delta = append_suffix_delta(base, result);
        write_pack_entry_header_kind(&mut pack, 7, delta.len() as u64);
        let base_oid = sley_core::object_id_for_bytes(format, "blob", base)
            .expect("test operation should succeed");
        pack.extend_from_slice(base_oid.as_bytes());
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&delta)
            .expect("test operation should succeed");
        pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));

        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    fn thin_ref_delta_pack_many(format: ObjectFormat, base: &[u8], results: &[&[u8]]) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(results.len() as u32).to_be_bytes());
        let base_oid = sley_core::object_id_for_bytes(format, "blob", base)
            .expect("test operation should succeed");
        for result in results {
            let delta = append_suffix_delta(base, result);
            write_pack_entry_header_kind(&mut pack, 7, delta.len() as u64);
            pack.extend_from_slice(base_oid.as_bytes());
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(&delta)
                .expect("test operation should succeed");
            pack.extend_from_slice(&encoder.finish().expect("test operation should succeed"));
        }
        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test operation should succeed")
            .as_nanos();
        std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
    }

    fn run_git_success(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("failed to run git {args:?}: {err}"));
        assert!(
            output.status.success(),
            "git {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn single_path_with_extension(dir: &Path, extension: &str) -> PathBuf {
        let mut paths = fs::read_dir(dir)
            .expect("test operation should succeed")
            .map(|entry| entry.expect("test operation should succeed").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(extension))
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 1, "expected one .{extension} file");
        paths.remove(0)
    }

    fn largest_path_with_extension(dir: &Path, extension: &str) -> PathBuf {
        fs::read_dir(dir)
            .expect("read fixture directory")
            .map(|entry| entry.expect("read fixture entry").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(extension))
            .max_by_key(|path| fs::metadata(path).expect("read fixture metadata").len())
            .expect("at least one fixture path")
    }

    fn pack_bitmap_index(
        format: ObjectFormat,
        object_count: u32,
        options: u16,
        pack_checksum: &ObjectId,
        entries: &[(u32, u8, u8, &[u64])],
        name_hash_cache: Option<&[u32]>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"BITM");
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&options.to_be_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        out.extend_from_slice(pack_checksum.as_bytes());
        write_test_ewah(&mut out, object_count, &[0b001]);
        write_test_ewah(&mut out, object_count, &[0b010]);
        write_test_ewah(&mut out, object_count, &[0b100]);
        write_test_ewah(&mut out, object_count, &[0]);
        for (position, xor_offset, flags, words) in entries {
            out.extend_from_slice(&position.to_be_bytes());
            out.push(*xor_offset);
            out.push(*flags);
            write_test_ewah(&mut out, object_count, words);
        }
        if let Some(cache) = name_hash_cache {
            for value in cache {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        let checksum =
            sley_core::digest_bytes(format, &out).expect("test operation should succeed");
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    fn write_test_ewah(out: &mut Vec<u8>, bit_size: u32, literals: &[u64]) {
        out.extend_from_slice(&bit_size.to_be_bytes());
        let words = ewah_literal_words(literals);
        out.extend_from_slice(&(words.len() as u32).to_be_bytes());
        for word in words {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out.extend_from_slice(&0u32.to_be_bytes());
    }

    fn ewah_literal_words(literals: &[u64]) -> Vec<u64> {
        let rlw = (literals.len() as u64) << 33;
        let mut words = vec![rlw];
        words.extend_from_slice(literals);
        words
    }

    fn refresh_trailing_checksum(format: ObjectFormat, bytes: &mut [u8]) {
        let checksum_offset = bytes.len() - format.raw_len();
        let checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])
            .expect("test operation should succeed");
        bytes[checksum_offset..].copy_from_slice(checksum.as_bytes());
    }

    fn append_suffix_delta(base: &[u8], result: &[u8]) -> Vec<u8> {
        assert!(result.starts_with(base));
        let suffix = &result[base.len()..];
        assert!(base.len() < 0x10000);
        assert!(suffix.len() < 0x80);
        let mut delta = Vec::new();
        write_delta_varint(&mut delta, base.len() as u64);
        write_delta_varint(&mut delta, result.len() as u64);
        delta.push(0x90);
        delta.push(base.len() as u8);
        delta.push(suffix.len() as u8);
        delta.extend_from_slice(suffix);
        delta
    }

    fn write_delta_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value as u8) & 0x7f;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn write_pack_entry_header_kind(out: &mut Vec<u8>, type_code: u8, mut size: u64) {
        let mut byte = (type_code << 4) | ((size as u8) & 0x0f);
        size >>= 4;
        if size != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        while size != 0 {
            let mut byte = (size as u8) & 0x7f;
            size >>= 7;
            if size != 0 {
                byte |= 0x80;
            }
            out.push(byte);
        }
    }

    fn write_ofs_delta_offset(out: &mut Vec<u8>, relative: usize) {
        assert!(relative < 0x80);
        out.push(relative as u8);
    }

    fn single_entry_index(
        format: ObjectFormat,
        oid: ObjectId,
        crc32: u32,
        offset: u32,
        pack_checksum: ObjectId,
    ) -> Vec<u8> {
        let mut index = Vec::new();
        index.extend_from_slice(&[0xff, b't', b'O', b'c']);
        index.extend_from_slice(&2u32.to_be_bytes());
        for idx in 0..256 {
            let count = if idx >= usize::from(oid.as_bytes()[0]) {
                1u32
            } else {
                0u32
            };
            index.extend_from_slice(&count.to_be_bytes());
        }
        index.extend_from_slice(oid.as_bytes());
        index.extend_from_slice(&crc32.to_be_bytes());
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(pack_checksum.as_bytes());
        let checksum =
            sley_core::digest_bytes(format, &index).expect("test operation should succeed");
        index.extend_from_slice(checksum.as_bytes());
        index
    }

    fn single_entry_index_v1(
        format: ObjectFormat,
        oid: ObjectId,
        offset: u32,
        pack_checksum: ObjectId,
    ) -> Vec<u8> {
        let mut index = Vec::new();
        for idx in 0..256 {
            let count = if idx >= usize::from(oid.as_bytes()[0]) {
                1u32
            } else {
                0u32
            };
            index.extend_from_slice(&count.to_be_bytes());
        }
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(oid.as_bytes());
        index.extend_from_slice(pack_checksum.as_bytes());
        let checksum =
            sley_core::digest_bytes(format, &index).expect("test operation should succeed");
        index.extend_from_slice(checksum.as_bytes());
        index
    }

    fn pack_reverse_index(
        format: ObjectFormat,
        positions: &[u32],
        pack_checksum: ObjectId,
    ) -> Vec<u8> {
        let mut reverse_index = Vec::new();
        reverse_index.extend_from_slice(b"RIDX");
        reverse_index.extend_from_slice(&1u32.to_be_bytes());
        reverse_index.extend_from_slice(&hash_function_id(format).to_be_bytes());
        for position in positions {
            reverse_index.extend_from_slice(&position.to_be_bytes());
        }
        reverse_index.extend_from_slice(pack_checksum.as_bytes());
        let checksum =
            sley_core::digest_bytes(format, &reverse_index).expect("test operation should succeed");
        reverse_index.extend_from_slice(checksum.as_bytes());
        reverse_index
    }

    fn pack_mtimes(format: ObjectFormat, mtimes: &[u32], pack_checksum: ObjectId) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"MTME");
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&hash_function_id(format).to_be_bytes());
        for mtime in mtimes {
            out.extend_from_slice(&mtime.to_be_bytes());
        }
        out.extend_from_slice(pack_checksum.as_bytes());
        let checksum =
            sley_core::digest_bytes(format, &out).expect("test operation should succeed");
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    fn midx_chunks_with_pack_names(
        _format: ObjectFormat,
        pack_names: Vec<u8>,
        entries: &[(ObjectId, u32, u64)],
    ) -> Vec<([u8; 4], Vec<u8>)> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let object_ids: Vec<ObjectId> = entries.iter().map(|entry| entry.0).collect();
        let mut large_offsets = Vec::new();
        let mut chunks = vec![
            (*b"PNAM", pack_names),
            (*b"OIDF", midx_oid_fanout(&object_ids)),
            (*b"OIDL", midx_oid_lookup(&object_ids)),
            (
                *b"OOFF",
                midx_ooff_entries(
                    &entries
                        .iter()
                        .map(|(_oid, pack_int_id, offset)| (*pack_int_id, *offset))
                        .collect::<Vec<_>>(),
                    &mut large_offsets,
                ),
            ),
        ];
        if !large_offsets.is_empty() {
            chunks.push((*b"LOFF", large_offsets));
        }
        chunks
    }

    fn midx_oid_fanout(object_ids: &[ObjectId]) -> Vec<u8> {
        let mut counts = [0u32; 256];
        for oid in object_ids {
            counts[oid.as_bytes()[0] as usize] += 1;
        }
        let mut running = 0u32;
        let mut out = Vec::new();
        for count in counts {
            running += count;
            out.extend_from_slice(&running.to_be_bytes());
        }
        out
    }

    fn midx_oid_lookup(object_ids: &[ObjectId]) -> Vec<u8> {
        let mut out = Vec::new();
        for oid in object_ids {
            out.extend_from_slice(oid.as_bytes());
        }
        out
    }

    fn midx_ooff_entries(entries: &[(u32, u64)], large_offsets: &mut Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        for (pack_int_id, offset) in entries {
            out.extend_from_slice(&pack_int_id.to_be_bytes());
            if *offset < 0x8000_0000 {
                out.extend_from_slice(&(*offset as u32).to_be_bytes());
            } else {
                let large_idx = (large_offsets.len() / 8) as u32;
                out.extend_from_slice(&(0x8000_0000 | large_idx).to_be_bytes());
                large_offsets.extend_from_slice(&offset.to_be_bytes());
            }
        }
        out
    }

    fn midx_u32_table(values: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn midx_bitmap_packs(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (bitmap_pos, bitmap_nr) in entries {
            out.extend_from_slice(&bitmap_pos.to_be_bytes());
            out.extend_from_slice(&bitmap_nr.to_be_bytes());
        }
        out
    }

    fn multi_pack_index(
        format: ObjectFormat,
        version: u8,
        pack_count: u32,
        chunks: &[([u8; 4], Vec<u8>)],
    ) -> Vec<u8> {
        let lookup_len = (chunks.len() + 1) * 12;
        let mut out = Vec::new();
        out.extend_from_slice(b"MIDX");
        out.push(version);
        out.push(hash_function_id(format) as u8);
        out.push(chunks.len() as u8);
        out.push(0);
        out.extend_from_slice(&pack_count.to_be_bytes());
        let mut chunk_offset = (12 + lookup_len) as u64;
        for (id, data) in chunks {
            out.extend_from_slice(id);
            out.extend_from_slice(&chunk_offset.to_be_bytes());
            chunk_offset += data.len() as u64;
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&chunk_offset.to_be_bytes());
        for (_id, data) in chunks {
            out.extend_from_slice(data);
        }
        let checksum =
            sley_core::digest_bytes(format, &out).expect("test operation should succeed");
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    // ---- EWAH encoder / bitmap writer tests ------------------------------

    fn pack_checksum_sha1() -> ObjectId {
        sley_core::digest_bytes(ObjectFormat::Sha1, b"pack").expect("test operation should succeed")
    }

    fn parse_ewah_bytes(bytes: &[u8]) -> EwahBitmap {
        // Wrap the EWAH body with the surrounding offset bookkeeping the parser
        // expects: a checksum offset that lies just past the serialised bitmap.
        let mut offset = 0usize;
        let checksum_offset = bytes.len();
        parse_bitmap_ewah(bytes, &mut offset, checksum_offset, 0)
            .expect("test operation should succeed")
    }

    #[test]
    fn ewah_encodes_single_literal_word_matching_helper() {
        // A bitmap whose only word is a literal must serialise as one RLW with
        // literal_len == 1 followed by the literal, identical to the test
        // helper used by the existing parser tests.
        let ewah = EwahBitmap::from_words(64, &[0b101]).expect("test operation should succeed");
        assert_eq!(ewah.words, ewah_literal_words(&[0b101]));
        assert_eq!(ewah.rlw_position, 0);
        assert_eq!(ewah.bit_size, 64);
    }

    #[test]
    fn ewah_byte_layout_is_big_endian() {
        let ewah = EwahBitmap::from_words(64, &[0x0102_0304_0506_0708])
            .expect("test operation should succeed");
        let bytes = ewah.to_bytes();
        let mut expected = Vec::new();
        expected.extend_from_slice(&64u32.to_be_bytes()); // bit_size
        expected.extend_from_slice(&2u32.to_be_bytes()); // word count: rlw + literal
        expected.extend_from_slice(&(1u64 << 33).to_be_bytes()); // rlw: literal_len = 1
        expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        expected.extend_from_slice(&0u32.to_be_bytes()); // rlw_position
        assert_eq!(bytes, expected);
    }

    #[test]
    fn ewah_empty_bitmap_serialises_like_git() {
        let ewah = EwahBitmap::empty();
        let bytes = ewah.to_bytes();
        // bit_size = 0, word_count = 0, rlw_position = 0.
        assert_eq!(bytes, vec![0u8; 12]);
        // It must still parse and decode to nothing.
        let parsed = parse_ewah_bytes(&bytes);
        assert_eq!(parsed, ewah);
        assert!(
            parsed
                .to_positions()
                .expect("test operation should succeed")
                .is_empty()
        );
    }

    #[test]
    fn ewah_compresses_clean_zero_run() {
        // Three all-zero words followed by a literal: the encoder should emit a
        // single RLW carrying a run of 3 clean-zero words plus one literal.
        let ewah =
            EwahBitmap::from_words(256, &[0, 0, 0, 0b1]).expect("test operation should succeed");
        assert_eq!(ewah.words.len(), 2, "expected one RLW plus one literal");
        let rlw = ewah.words[0];
        assert_eq!(rlw & 1, 0, "run bit should be zero");
        assert_eq!((rlw >> 1) & 0xffff_ffff, 3, "run length should be 3");
        assert_eq!(rlw >> 33, 1, "literal length should be 1");
        assert_eq!(ewah.words[1], 0b1);
    }

    #[test]
    fn ewah_compresses_clean_ones_run() {
        let ewah = EwahBitmap::from_words(192, &[u64::MAX, u64::MAX, u64::MAX])
            .expect("test operation should succeed");
        // Pure run of ones, no literals: one RLW only.
        assert_eq!(ewah.words.len(), 1);
        let rlw = ewah.words[0];
        assert_eq!(rlw & 1, 1, "run bit should be one");
        assert_eq!((rlw >> 1) & 0xffff_ffff, 3, "run length should be 3");
        assert_eq!(rlw >> 33, 0, "no literals");
    }

    #[test]
    fn ewah_run_then_literal_then_run_roundtrips() {
        let words = vec![0, 0, 0xdead_beef, u64::MAX, u64::MAX, 0, 0xabc];
        let bit_size = (words.len() * 64) as u32;
        let ewah = EwahBitmap::from_words(bit_size, &words).expect("test operation should succeed");
        assert_eq!(
            ewah.to_words().expect("test operation should succeed"),
            words
        );
    }

    #[test]
    fn ewah_drops_trailing_clean_zero_words() {
        // Trailing all-zero words beyond a literal carry no information and git
        // does not serialise them, but to_words() restores them up to bit_size.
        let words = vec![0b1, 0, 0, 0];
        let ewah = EwahBitmap::from_words(1, &words).expect("test operation should succeed");
        // bit_size of 1 means a single backing word.
        assert_eq!(ewah.bit_size, 1);
        assert_eq!(
            ewah.to_words().expect("test operation should succeed"),
            vec![0b1]
        );
    }

    #[test]
    fn ewah_from_positions_roundtrips_via_positions() {
        let positions = [0u32, 1, 63, 64, 65, 200, 511];
        let ewah =
            EwahBitmap::from_positions(512, &positions).expect("test operation should succeed");
        let mut decoded = ewah.to_positions().expect("test operation should succeed");
        decoded.sort_unstable();
        assert_eq!(decoded, positions);
    }

    #[test]
    fn ewah_from_positions_dedupes_and_orders() {
        let ewah = EwahBitmap::from_positions(128, &[100, 5, 100, 5, 5])
            .expect("test operation should succeed");
        assert_eq!(
            ewah.to_positions().expect("test operation should succeed"),
            vec![5, 100]
        );
    }

    #[test]
    fn ewah_huge_zero_run_spans_multiple_rlws() {
        // A run longer than the 32-bit running-length field forces the encoder
        // to emit more than one RLW. Use one literal bit far out, with a bit
        // size large enough to exceed u32::MAX clean words is impractical, so
        // assert the field arithmetic via a direct builder run instead.
        let mut builder = EwahBuilder::new(0);
        builder.add_empty_words(false, 0xffff_ffff);
        builder.add_empty_words(false, 5);
        let ewah = builder.finish().expect("test operation should succeed");
        assert_eq!(ewah.words.len(), 2, "run split across two RLWs");
        assert_eq!((ewah.words[0] >> 1) & 0xffff_ffff, 0xffff_ffff);
        assert_eq!(ewah.words[1] & 1, 0);
        assert_eq!((ewah.words[1] >> 1) & 0xffff_ffff, 5);
        assert_eq!(ewah.rlw_position, 1);
    }

    #[test]
    fn ewah_from_words_rejects_oversized_bit_size() {
        // bit_size demands two words but only one is supplied.
        assert!(EwahBitmap::from_words(65, &[0]).is_err());
    }

    #[test]
    fn ewah_from_positions_rejects_out_of_range() {
        assert!(EwahBitmap::from_positions(64, &[64]).is_err());
    }

    #[test]
    fn ewah_serialised_bytes_reparse_to_equal_bitmap() {
        // Exercise the full encode -> serialise -> parse loop for a non-trivial
        // pattern and assert structural equality against the parser's model.
        let words = vec![0, u64::MAX, 0x1234_5678_9abc_def0, 0, 0, 0xff];
        let bit_size = (words.len() * 64) as u32;
        let ewah = EwahBitmap::from_words(bit_size, &words).expect("test operation should succeed");
        let bytes = ewah.to_bytes();
        let parsed = parse_ewah_bytes(&bytes);
        assert_eq!(parsed, ewah);
        assert_eq!(
            parsed.to_words().expect("test operation should succeed"),
            words
        );
    }

    #[test]
    fn pack_bitmap_index_write_parse_roundtrip_sha1() {
        // commit, tree, blob in pack order; one selected commit reaching all.
        let object_types = [ObjectType::Commit, ObjectType::Tree, ObjectType::Blob];
        let bytes = write_bitmap(
            ObjectFormat::Sha1,
            pack_checksum_sha1(),
            &object_types,
            &[(0u32, 0u32, vec![1u32, 2u32])],
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(&bytes[..4], b"BITM");

        let parsed = PackBitmapIndex::parse(&bytes, ObjectFormat::Sha1, 3)
            .expect("test operation should succeed");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.options, PackBitmapIndex::OPTION_FULL_DAG);
        assert_eq!(parsed.pack_checksum, pack_checksum_sha1());
        assert_eq!(
            parsed
                .type_bitmaps
                .commits
                .to_positions()
                .expect("test operation should succeed"),
            vec![0]
        );
        assert_eq!(
            parsed
                .type_bitmaps
                .trees
                .to_positions()
                .expect("test operation should succeed"),
            vec![1]
        );
        assert_eq!(
            parsed
                .type_bitmaps
                .blobs
                .to_positions()
                .expect("test operation should succeed"),
            vec![2]
        );
        assert!(
            parsed
                .type_bitmaps
                .tags
                .to_positions()
                .expect("test operation should succeed")
                .is_empty()
        );
        assert_eq!(parsed.entries.len(), 1);
        let entry = parsed
            .entry_for_index_position(0)
            .expect("test operation should succeed");
        assert_eq!(entry.xor_offset, 0);
        assert_eq!(entry.flags, 0);
        assert_eq!(
            entry
                .bitmap
                .to_positions()
                .expect("test operation should succeed"),
            vec![0, 1, 2]
        );
        assert_eq!(parsed.name_hash_cache, None);
    }

    #[test]
    fn pack_bitmap_index_write_parse_roundtrip_sha256() {
        let pack_checksum = sley_core::digest_bytes(ObjectFormat::Sha256, b"pack")
            .expect("test operation should succeed");
        let object_types = [ObjectType::Commit, ObjectType::Tree];
        let bytes = write_bitmap(
            ObjectFormat::Sha256,
            pack_checksum.clone(),
            &object_types,
            &[(0u32, 0u32, vec![1u32])],
            None,
        )
        .expect("test operation should succeed");
        let parsed = PackBitmapIndex::parse(&bytes, ObjectFormat::Sha256, 2)
            .expect("test operation should succeed");
        assert_eq!(parsed.format, ObjectFormat::Sha256);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.index_checksum.format(), ObjectFormat::Sha256);
        assert_eq!(
            parsed.entries[0]
                .bitmap
                .to_positions()
                .expect("test operation should succeed"),
            vec![0, 1]
        );
    }

    #[test]
    fn pack_bitmap_index_write_includes_name_hash_cache() {
        let object_types = [ObjectType::Commit, ObjectType::Tree, ObjectType::Blob];
        let cache = vec![0x1111_1111u32, 0x2222_2222, 0x3333_3333];
        let bytes = write_bitmap(
            ObjectFormat::Sha1,
            pack_checksum_sha1(),
            &object_types,
            &[(0u32, 0u32, vec![1u32, 2u32])],
            Some(cache.clone()),
        )
        .expect("test operation should succeed");
        let parsed = PackBitmapIndex::parse(&bytes, ObjectFormat::Sha1, 3)
            .expect("test operation should succeed");
        assert_eq!(
            parsed.options,
            PackBitmapIndex::OPTION_FULL_DAG | PackBitmapIndex::OPTION_HASH_CACHE
        );
        assert_eq!(parsed.name_hash_cache, Some(cache));
    }

    #[test]
    fn pack_bitmap_writer_supports_multiple_commits() {
        let object_types = [
            ObjectType::Commit,
            ObjectType::Commit,
            ObjectType::Tree,
            ObjectType::Blob,
        ];
        let mut writer =
            PackBitmapWriter::new(ObjectFormat::Sha1, pack_checksum_sha1(), &object_types)
                .expect("test operation should succeed");
        writer
            .add_commit(0, 0, &[2, 3])
            .expect("test operation should succeed");
        writer
            .add_commit(1, 1, &[2])
            .expect("test operation should succeed");
        let bytes = writer.write().expect("test operation should succeed");
        let parsed = PackBitmapIndex::parse(&bytes, ObjectFormat::Sha1, 4)
            .expect("test operation should succeed");
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(
            parsed
                .type_bitmaps
                .commits
                .to_positions()
                .expect("test operation should succeed"),
            vec![0, 1]
        );
        let first = parsed
            .entry_for_index_position(0)
            .expect("test operation should succeed");
        assert_eq!(
            first
                .bitmap
                .to_positions()
                .expect("test operation should succeed"),
            vec![0, 2, 3]
        );
        let second = parsed
            .entry_for_index_position(1)
            .expect("test operation should succeed");
        assert_eq!(
            second
                .bitmap
                .to_positions()
                .expect("test operation should succeed"),
            vec![1, 2]
        );
    }

    #[test]
    fn pack_bitmap_writer_roundtrips_lookup_table() {
        let object_types = [ObjectType::Commit, ObjectType::Commit, ObjectType::Tree];
        let mut writer =
            PackBitmapWriter::new(ObjectFormat::Sha1, pack_checksum_sha1(), &object_types)
                .expect("test operation should succeed")
                .with_lookup_table(true);
        writer
            .add_commit(0, 1, &[2])
            .expect("test operation should succeed");
        writer
            .add_commit(1, 0, &[2])
            .expect("test operation should succeed");
        let bytes = writer.write().expect("test operation should succeed");
        let parsed = PackBitmapIndex::parse(&bytes, ObjectFormat::Sha1, 3)
            .expect("test operation should succeed");
        assert!(parsed.lookup_table);
        assert_ne!(parsed.options & PackBitmapIndex::OPTION_LOOKUP_TABLE, 0);
        assert_eq!(parsed.entries.len(), 2);
    }

    #[test]
    fn pack_bitmap_index_recomputes_checksum_on_write() {
        // The provided index_checksum field is ignored; write recomputes it so
        // a bogus placeholder still produces a valid, parseable file.
        let object_types = [ObjectType::Commit, ObjectType::Blob];
        let writer = PackBitmapWriter::new(ObjectFormat::Sha1, pack_checksum_sha1(), &object_types)
            .expect("test operation should succeed");
        let mut index = writer.build().expect("test operation should succeed");
        // build() sets an all-zero placeholder checksum.
        assert_eq!(index.index_checksum.as_bytes(), [0u8; 20]);
        index.entries.clear(); // mutate the model after build
        index.entries.push(PackBitmapEntry {
            object_position: 0,
            xor_offset: 0,
            flags: 0,
            bitmap: EwahBitmap::from_positions(2, &[0, 1]).expect("test operation should succeed"),
        });
        let bytes = index.write().expect("test operation should succeed");
        // Parsing validates the trailing checksum, so a wrong checksum fails.
        let parsed = PackBitmapIndex::parse(&bytes, ObjectFormat::Sha1, 2)
            .expect("test operation should succeed");
        assert_ne!(parsed.index_checksum.as_bytes(), [0u8; 20]);
    }

    #[test]
    fn pack_bitmap_writer_rejects_non_commit_selection() {
        let object_types = [ObjectType::Commit, ObjectType::Blob];
        let mut writer =
            PackBitmapWriter::new(ObjectFormat::Sha1, pack_checksum_sha1(), &object_types)
                .expect("test operation should succeed");
        // Position 1 is a blob, not a commit.
        assert!(writer.add_commit(1, 1, &[]).is_err());
        // Position 5 is out of range entirely.
        assert!(writer.add_commit(5, 5, &[]).is_err());
        // Index position out of range.
        assert!(writer.add_commit(0, 5, &[]).is_err());
        // Reachable position out of range.
        assert!(writer.add_commit(0, 0, &[9]).is_err());
    }

    #[test]
    fn pack_bitmap_writer_rejects_checksum_format_mismatch() {
        let sha256_checksum = sley_core::digest_bytes(ObjectFormat::Sha256, b"pack")
            .expect("test operation should succeed");
        assert!(
            PackBitmapWriter::new(ObjectFormat::Sha1, sha256_checksum, &[ObjectType::Commit])
                .is_err()
        );
    }

    #[test]
    fn pack_bitmap_writer_rejects_bad_name_hash_cache_len() {
        let writer = PackBitmapWriter::new(
            ObjectFormat::Sha1,
            pack_checksum_sha1(),
            &[ObjectType::Commit],
        )
        .expect("test operation should succeed");
        assert!(writer.with_name_hash_cache(vec![1, 2]).is_err());
    }

    #[test]
    fn pack_bitmap_index_write_rejects_inconsistent_cache_flag() {
        let mut index = PackBitmapWriter::new(
            ObjectFormat::Sha1,
            pack_checksum_sha1(),
            &[ObjectType::Commit],
        )
        .expect("test operation should succeed")
        .build()
        .expect("test operation should succeed");
        // Flag set but no cache present.
        index.options |= PackBitmapIndex::OPTION_HASH_CACHE;
        assert!(index.write().is_err());
        // Cache present but flag missing.
        index.options = PackBitmapIndex::OPTION_FULL_DAG;
        index.name_hash_cache = Some(vec![0]);
        assert!(index.write().is_err());
    }

    #[test]
    fn write_bitmap_roundtrips_through_upstream_git_parser() {
        // Build a real pack with git, then overwrite reachability with our own
        // writer using the real pack checksum and object types, and confirm our
        // bytes parse under the same parser that reads upstream bitmaps.
        let root = unique_temp_dir("git-pack-bitmap-writer");
        fs::create_dir_all(&root).expect("test operation should succeed");
        {
            run_git_success(&root, &["init", "-q", "-b", "main"]);
            run_git_success(
                &root,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-q",
                    "-m",
                    "one",
                ],
            );
            run_git_success(&root, &["repack", "-adb"]);
            let pack_dir = root.join(".git").join("objects").join("pack");
            let idx_path = single_path_with_extension(&pack_dir, "idx");
            let index = PackIndex::parse(
                &fs::read(idx_path).expect("test operation should succeed"),
                ObjectFormat::Sha1,
            )
            .expect("test operation should succeed");
            // Read object types from the pack so the type bitmaps are accurate.
            let pack_path = single_path_with_extension(&pack_dir, "pack");
            let pack =
                PackFile::parse_sha1(&fs::read(pack_path).expect("test operation should succeed"))
                    .expect("test operation should succeed");
            // Map each index entry (sorted by oid) to its pack offset, then to a
            // pack-order position so positions line up with the index ordering.
            let mut offsets: Vec<u64> = index.entries.iter().map(|entry| entry.offset).collect();
            offsets.sort_unstable();
            let position_of = |offset: u64| -> u32 {
                offsets
                    .iter()
                    .position(|value| *value == offset)
                    .expect("test operation should succeed") as u32
            };
            let mut object_types = vec![ObjectType::Blob; index.entries.len()];
            for entry in &index.entries {
                let position = position_of(entry.offset) as usize;
                // Find the parsed object at this pack offset to read its type.
                if let Some(parsed) = pack
                    .entries
                    .iter()
                    .find(|po| po.entry.offset == entry.offset)
                {
                    object_types[position] = parsed.object.object_type;
                }
            }
            // Select the first commit position we find and reach everything.
            let commit_position = object_types
                .iter()
                .position(|ty| *ty == ObjectType::Commit)
                .expect("test operation should succeed") as u32;
            // The entry records the commit's position in the oid-sorted index.
            let commit_index_position = index
                .entries
                .iter()
                .position(|entry| position_of(entry.offset) == commit_position)
                .expect("test operation should succeed")
                as u32;
            let reachable: Vec<u32> = (0..index.entries.len() as u32).collect();
            let bytes = write_bitmap(
                ObjectFormat::Sha1,
                index.pack_checksum.clone(),
                &object_types,
                &[(commit_position, commit_index_position, reachable)],
                None,
            )
            .expect("test operation should succeed");
            let parsed = PackBitmapIndex::parse(&bytes, ObjectFormat::Sha1, index.entries.len())
                .expect("test operation should succeed");
            assert_eq!(parsed.pack_checksum, index.pack_checksum);
            assert_eq!(parsed.entries.len(), 1);
            assert_eq!(
                parsed.entries[0]
                    .bitmap
                    .to_positions()
                    .expect("test operation should succeed")
                    .len(),
                index.entries.len()
            );
        };
        let _ = fs::remove_dir_all(&root);
    }

    // ---- sley#4 / sley#5: bounds on untrusted pack input --------------------

    fn zlib_compress(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(bytes)
            .expect("test operation should succeed");
        encoder.finish().expect("test operation should succeed")
    }

    /// A structurally valid 32-byte pack — correct signature, version, and
    /// trailing checksum — whose object-count field says `declared`.
    fn pack_with_declared_object_count(declared: u32) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&declared.to_be_bytes());
        let checksum = sley_core::digest_bytes(ObjectFormat::Sha1, &pack)
            .expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    /// Regression (sley#4): the pack header's 32-bit object count is
    /// attacker-controlled and went straight to `Vec::with_capacity`. A 32-byte
    /// pack declaring `u32::MAX` objects asks the allocator for ~480 GiB before
    /// a single entry is inspected; that allocation fails and Rust's OOM handler
    /// calls `abort()`. Hence the child process: `abort()` takes the whole test
    /// binary with it, so neither `JoinHandle::join` nor `catch_unwind` can
    /// observe it the way they observe the sley#35 delta bomb.
    #[test]
    fn rejects_absurd_declared_object_count_without_preallocating() {
        const CHILD_ENV: &str = "SLEY_PACK_OBJECT_COUNT_BOMB_CHILD";
        const TEST_PATH: &str = "tests::rejects_absurd_declared_object_count_without_preallocating";

        if std::env::var_os(CHILD_ENV).is_some() {
            for declared in [u32::MAX, 1 << 30, 1 << 24] {
                let pack = pack_with_declared_object_count(declared);
                assert!(
                    PackFile::parse_sha1(&pack).is_err(),
                    "a 32-byte pack declaring {declared} objects must be rejected"
                );
                assert!(
                    PackIndex::write_v2_for_pack(&pack, ObjectFormat::Sha1).is_err(),
                    "index-pack must reject {declared} objects in a 32-byte pack too"
                );
                assert!(
                    PackFile::verify_pack_stats(&pack, ObjectFormat::Sha1).is_err(),
                    "verify-pack must reject {declared} objects in a 32-byte pack too"
                );
            }
            return;
        }

        let exe = std::env::current_exe().expect("test binary path should be available");
        let status = Command::new(exe)
            .args(["--exact", TEST_PATH, "--nocapture"])
            .env(CHILD_ENV, "1")
            .status()
            .expect("re-running the test binary should succeed");
        assert!(
            status.success(),
            "child process died ({status:?}) parsing packs with oversized declared \
             object counts — the declared count reached an allocation instead of a \
             bounds check"
        );
    }

    /// A pack that declares a merely implausible count — not one large enough
    /// to abort the process — is still rejected by the header check rather than
    /// part-way through the entry loop.
    #[test]
    fn rejects_declared_object_count_larger_than_pack_can_hold() {
        let pack = pack_with_declared_object_count(64);
        let error = PackFile::parse_sha1(&pack).expect_err("declared count must be rejected");
        assert!(
            format!("{error}").contains("only has room for"),
            "expected a declared-count bound error, got: {error}"
        );
    }

    /// A delta that ignores its base and emits `result` as a single insert
    /// instruction. Every link of a chain built from this is the same tiny
    /// size, so chain *depth* is the only variable under test.
    fn literal_delta(base_len: usize, result: &[u8]) -> Vec<u8> {
        assert!(result.len() <= 0x7f, "insert instruction size is one byte");
        let mut delta = Vec::new();
        write_delta_varint(&mut delta, base_len as u64);
        write_delta_varint(&mut delta, result.len() as u64);
        delta.push(result.len() as u8);
        delta.extend_from_slice(result);
        delta
    }

    /// Distinct 8-byte bodies, so no two links of a chain share an object id.
    fn chain_bodies(depth: usize) -> Vec<Vec<u8>> {
        (0..=depth)
            .map(|idx| format!("{idx:08}").into_bytes())
            .collect()
    }

    /// One `ofs-delta` chain of `depth` links on top of a single blob, laid out
    /// front to back the way a real packer emits it.
    fn ofs_delta_chain_pack(format: ObjectFormat, depth: usize) -> Vec<u8> {
        let bodies = chain_bodies(depth);
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(depth as u32 + 1).to_be_bytes());

        let mut base_offset = pack.len();
        write_entry_header(&mut pack, ObjectType::Blob, bodies[0].len() as u64);
        pack.extend_from_slice(&zlib_compress(&bodies[0]));

        for idx in 1..=depth {
            let delta = literal_delta(bodies[idx - 1].len(), &bodies[idx]);
            let entry_offset = pack.len();
            write_pack_entry_header_kind(&mut pack, 6, delta.len() as u64);
            write_ofs_delta_offset(&mut pack, entry_offset - base_offset);
            pack.extend_from_slice(&zlib_compress(&delta));
            base_offset = entry_offset;
        }

        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    /// One `ref-delta` chain of `depth` links on top of a single blob. When
    /// `reversed`, the deltas are laid out deepest-first — legal for ref-deltas
    /// (unlike ofs-deltas, whose base must precede them) and the adversarial
    /// shape from sley#5: a naive resolver advances the chain by only one link
    /// per full-list scan, so an unbounded chain costs one scan per link.
    fn ref_delta_chain_pack(format: ObjectFormat, depth: usize, reversed: bool) -> Vec<u8> {
        let bodies = chain_bodies(depth);

        let mut base_entry = Vec::new();
        write_entry_header(&mut base_entry, ObjectType::Blob, bodies[0].len() as u64);
        base_entry.extend_from_slice(&zlib_compress(&bodies[0]));

        let mut delta_entries = Vec::with_capacity(depth);
        for idx in 1..=depth {
            let delta = literal_delta(bodies[idx - 1].len(), &bodies[idx]);
            let base_oid = sley_core::object_id_for_bytes(format, "blob", &bodies[idx - 1])
                .expect("test operation should succeed");
            let mut entry = Vec::new();
            write_pack_entry_header_kind(&mut entry, 7, delta.len() as u64);
            entry.extend_from_slice(base_oid.as_bytes());
            entry.extend_from_slice(&zlib_compress(&delta));
            delta_entries.push(entry);
        }
        if reversed {
            delta_entries.reverse();
        }

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(depth as u32 + 1).to_be_bytes());
        if !reversed {
            pack.extend_from_slice(&base_entry);
        }
        for entry in &delta_entries {
            pack.extend_from_slice(entry);
        }
        if reversed {
            pack.extend_from_slice(&base_entry);
        }

        let checksum =
            sley_core::digest_bytes(format, &pack).expect("test operation should succeed");
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    #[test]
    fn header_read_bounds_ofs_delta_recursion_at_the_shared_ceiling() {
        let at_ceiling = ofs_delta_chain_pack(ObjectFormat::Sha1, MAX_READ_DELTA_CHAIN_DEPTH);
        let at_ceiling_offset = pack_entry_descriptors(&at_ceiling, ObjectFormat::Sha1)
            .last()
            .expect("the pack has entries")
            .offset;
        let header = read_object_header_at(
            &at_ceiling,
            at_ceiling_offset,
            ObjectFormat::Sha1,
            0,
            |_, _| Ok(None),
        )
        .expect("a header chain at the ceiling must resolve");
        assert_eq!(header.type_and_size(), (ObjectType::Blob, 8));

        let over_ceiling = ofs_delta_chain_pack(ObjectFormat::Sha1, 5_000);
        let over_ceiling_offset = pack_entry_descriptors(&over_ceiling, ObjectFormat::Sha1)
            .last()
            .expect("the pack has entries")
            .offset;
        let error = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                read_object_header_at(
                    &over_ceiling,
                    over_ceiling_offset,
                    ObjectFormat::Sha1,
                    0,
                    |_, _| Ok(None),
                )
                .expect_err("a header chain past the ceiling must be rejected")
            })
            .expect("spawn small-stack thread")
            .join()
            .expect("the bounded header walk must not overflow its stack");
        let rejected_depth = MAX_READ_DELTA_CHAIN_DEPTH + 1;
        assert!(
            error
                .to_string()
                .contains(&format!("observed depth {rejected_depth}")),
            "expected an actionable depth error, got: {error}"
        );
    }

    #[test]
    fn header_read_combines_prior_and_local_delta_depth() {
        let pack = ofs_delta_chain_pack(ObjectFormat::Sha1, 2);
        let offset = pack_entry_descriptors(&pack, ObjectFormat::Sha1)
            .last()
            .expect("the pack has entries")
            .offset;
        let error = read_object_header_at(
            &pack,
            offset,
            ObjectFormat::Sha1,
            MAX_READ_DELTA_CHAIN_DEPTH - 1,
            |_, _| Ok(None),
        )
        .expect_err("local ofs-deltas must count prior cross-pack delta links");
        let rejected_depth = MAX_READ_DELTA_CHAIN_DEPTH + 1;
        assert!(
            error
                .to_string()
                .contains(&format!("observed depth {rejected_depth}")),
            "expected the combined depth in the error, got: {error}"
        );
    }

    #[test]
    fn header_cache_preserves_the_cumulative_depth_limit() {
        let pack = ofs_delta_chain_pack(ObjectFormat::Sha1, 3);
        let descriptors = pack_entry_descriptors(&pack, ObjectFormat::Sha1);
        let target_offset = descriptors[3].offset;
        let cached_base_offset = descriptors[2].offset;
        let initial_depth = MAX_READ_DELTA_CHAIN_DEPTH - 2;
        let rejected_depth = MAX_READ_DELTA_CHAIN_DEPTH + 1;

        let mut direct_cache = MapHeaderTypeCache::default();
        read_object_header_at_with_cache(
            &pack,
            target_offset,
            ObjectFormat::Sha1,
            0,
            |_, _| Ok(None),
            &mut direct_cache,
        )
        .expect("the top-level read should warm the target header");
        let direct_error = read_object_header_at_with_cache(
            &pack,
            target_offset,
            ObjectFormat::Sha1,
            initial_depth,
            |_, _| Ok(None),
            &mut direct_cache,
        )
        .expect_err("a direct cache hit must include its represented chain depth");
        assert!(
            direct_error
                .to_string()
                .contains(&format!("observed depth {rejected_depth}")),
            "expected the direct cached depth in the error, got: {direct_error}"
        );

        let mut base_cache = MapHeaderTypeCache::default();
        read_object_header_at_with_cache(
            &pack,
            cached_base_offset,
            ObjectFormat::Sha1,
            0,
            |_, _| Ok(None),
            &mut base_cache,
        )
        .expect("the top-level read should warm the target's immediate base");
        let base_error = read_object_header_at_with_cache(
            &pack,
            target_offset,
            ObjectFormat::Sha1,
            initial_depth,
            |_, _| Ok(None),
            &mut base_cache,
        )
        .expect_err("an internal base-cache hit must include its represented chain depth");
        assert!(
            base_error
                .to_string()
                .contains(&format!("observed depth {rejected_depth}")),
            "expected the cached base depth in the error, got: {base_error}"
        );
    }

    /// A chain exactly at the ceiling still resolves: the bound must not reject
    /// anything sley's own writer (or git at its `pack.depth` default) emits.
    #[test]
    fn resolves_delta_chain_at_the_depth_ceiling() {
        for pack in [
            ofs_delta_chain_pack(ObjectFormat::Sha1, DEFAULT_PACK_DEPTH),
            ref_delta_chain_pack(ObjectFormat::Sha1, DEFAULT_PACK_DEPTH, false),
        ] {
            let parsed = PackFile::parse_sha1(&pack).expect("a chain at the ceiling must resolve");
            assert_eq!(parsed.entries.len(), DEFAULT_PACK_DEPTH + 1);
        }
    }

    #[test]
    fn configured_delta_chain_depth_limit_allows_deeper_pack() {
        let limits = PackReadLimits {
            max_delta_depth: 60,
            ..PackReadLimits::default()
        };
        let pack = ofs_delta_chain_pack(ObjectFormat::Sha1, 60);

        let parsed = PackFile::parse_with_limits(&pack, ObjectFormat::Sha1, limits)
            .expect("a chain at the configured ceiling must resolve");
        assert_eq!(parsed.entries.len(), 61);

        let indexed = PackIndex::write_v2_for_pack_with_limits(&pack, ObjectFormat::Sha1, limits)
            .expect("the in-memory indexer must use the configured ceiling");
        assert_eq!(indexed.entries.len(), 61);
    }

    #[test]
    fn configured_delta_chain_depth_limit_remains_finite_and_actionable() {
        let limits = PackReadLimits {
            max_delta_depth: 60,
            ..PackReadLimits::default()
        };
        let pack = ofs_delta_chain_pack(ObjectFormat::Sha1, 61);

        let error = PackFile::parse_with_limits(&pack, ObjectFormat::Sha1, limits)
            .expect_err("a chain past the configured ceiling must be rejected");
        let message = format!("{error}");

        assert!(
            message.contains("observed depth 61"),
            "expected the observed depth in the error, got: {error}"
        );
        assert!(
            message.contains("configured limit 60"),
            "expected the configured limit in the error, got: {error}"
        );
        assert!(
            message.contains("PackReadLimits::max_delta_depth")
                && message.contains("git repack --depth="),
            "expected actionable remedies in the error, got: {error}"
        );
    }

    /// Regression (sley#5): the read path enforced no chain-depth limit at all.
    /// `DEFAULT_PACK_DEPTH` existed but was consulted only when *writing*.
    #[test]
    fn rejects_delta_chain_deeper_than_the_ceiling() {
        for pack in [
            ofs_delta_chain_pack(ObjectFormat::Sha1, DEFAULT_PACK_DEPTH + 1),
            ref_delta_chain_pack(ObjectFormat::Sha1, DEFAULT_PACK_DEPTH + 1, false),
        ] {
            let error =
                PackFile::parse_sha1(&pack).expect_err("a chain past the ceiling must be rejected");
            assert!(
                format!("{error}").contains("exceeds maximum depth"),
                "expected a chain-depth error, got: {error}"
            );
        }
    }

    /// Regression (sley#5): a long chain laid out back to front made the former
    /// resolver run one full pass per link — O(N^2) scans on top of O(N) delta
    /// applications — with nothing to stop it. The dependency-level resolver
    /// rejects the chain at the configured depth ceiling.
    #[test]
    fn rejects_adversarially_ordered_long_delta_chain_promptly() {
        let pack = ref_delta_chain_pack(ObjectFormat::Sha1, 5_000, true);
        let started = std::time::Instant::now();
        let error = PackFile::parse_sha1(&pack)
            .expect_err("a long adversarial chain must be rejected, not resolved");
        let elapsed = started.elapsed();
        assert!(
            format!("{error}").contains("exceeds maximum depth"),
            "expected a chain-depth error, got: {error}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "rejection took {elapsed:?}; resolution is still doing work proportional \
             to the chain length"
        );
    }

    /// `index-pack` resolves through the same helper, so it inherits the same
    /// ceiling — the bound belongs to the shared resolver, not to one caller.
    #[test]
    fn index_pack_rejects_delta_chain_deeper_than_the_ceiling() {
        let pack = ofs_delta_chain_pack(ObjectFormat::Sha1, DEFAULT_PACK_DEPTH + 1);
        let error = PackIndex::write_v2_for_pack(&pack, ObjectFormat::Sha1)
            .expect_err("index-pack must reject a chain past the ceiling");
        assert!(
            format!("{error}").contains("exceeds maximum depth"),
            "expected a chain-depth error, got: {error}"
        );
    }
}
