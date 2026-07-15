use sley_core::{AtomicCancel, CancelFlag, GitError, ObjectFormat, ObjectId};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{
    BoundedPackDecoder, PackFile, PackLimitKind, PackReadError, PackReadLimits, PackReadSource,
    PackWriteOptions, RefDeltaBase, SlicePackSource,
};
use std::collections::HashMap;
use std::fs;

#[test]
fn bounded_targeted_read_matches_existing_decoder_and_reports_usage() {
    let expected = EncodedObject::new(ObjectType::Blob, vec![b'x'; 32 * 1024]);
    let written = PackFile::write_undeltified(std::slice::from_ref(&expected), ObjectFormat::Sha1)
        .expect("write pack");
    let limits = PackReadLimits {
        max_delta_depth: 8,
        max_materialized_bytes: 128 * 1024,
        max_cached_bytes: 64 * 1024,
    };
    let source = SlicePackSource::new(&written.pack);
    let mut decoder =
        BoundedPackDecoder::new(source, ObjectFormat::Sha1, limits).expect("open decoder");

    let decoded = decoder
        .read_object_at(written.entries[0].offset, |_| Ok(None), |_, _, _| Ok(()))
        .expect("decode targeted object");

    assert_eq!(*decoded.object, expected);
    assert!(decoded.stats.compressed_bytes_read > 0);
    assert_eq!(decoded.stats.cached_bytes, decoded.object.body.capacity());
    assert!(decoded.stats.peak_materialized_bytes >= decoded.object.body.capacity());
    assert!(decoded.stats.peak_materialized_bytes <= limits.max_materialized_bytes);
    assert!(decoded.stats.cached_bytes <= limits.max_cached_bytes);
}

fn similar_objects(count: usize, body_len: usize) -> Vec<EncodedObject> {
    (0..count)
        .map(|index| {
            let mut body = vec![b'x'; body_len];
            body[body_len - 4..].copy_from_slice(&(index as u32).to_be_bytes());
            EncodedObject::new(ObjectType::Blob, body)
        })
        .collect()
}

fn generous_limits() -> PackReadLimits {
    PackReadLimits {
        max_delta_depth: 128,
        max_materialized_bytes: 256 * 1024,
        max_cached_bytes: 64 * 1024,
    }
}

#[test]
fn bounded_decoder_matches_existing_decoder_for_ofs_and_in_pack_ref_deltas() {
    let objects = similar_objects(12, 16 * 1024);
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        for prefer_ofs_delta in [true, false] {
            let options = PackWriteOptions::new()
                .with_window(1)
                .with_depth(32)
                .with_reorder(false)
                .with_prefer_ofs_delta(prefer_ofs_delta);
            let written = PackFile::write_packed_with_options(&objects, format, &options)
                .expect("write deltified pack");
            assert!(written.delta_count > 0, "fixture must contain deltas");
            let parsed = PackFile::parse(&written.pack, format).expect("parse ground truth");
            let offsets: HashMap<ObjectId, u64> = written
                .entries
                .iter()
                .map(|entry| (entry.oid, entry.offset))
                .collect();
            let limits = PackReadLimits {
                max_delta_depth: 32,
                max_materialized_bytes: 64 * 1024,
                max_cached_bytes: 20 * 1024,
            };
            let mut decoder =
                BoundedPackDecoder::new(SlicePackSource::new(&written.pack), format, limits)
                    .expect("open decoder");

            let mut delta_reads = 0;
            // Reverse order makes the first read traverse the complete chain before
            // any possible base has been warmed in the cache.
            for expected in parsed.entries.iter().rev() {
                let decoded = decoder
                    .read_object_at(
                        expected.entry.offset,
                        |oid| Ok(offsets.get(oid).copied().map(RefDeltaBase::InPack)),
                        |_, _, _| Ok(()),
                    )
                    .expect("decode object");
                assert_eq!(*decoded.object, expected.object);
                delta_reads += usize::from(decoded.stats.delta_depth > 0);
                assert!(decoded.stats.peak_materialized_bytes <= limits.max_materialized_bytes);
                assert!(decoded.stats.cached_bytes <= limits.max_cached_bytes);
            }
            assert!(
                delta_reads > 5,
                "multiple targeted reads must be delta-backed"
            );
        }
    }
}

#[test]
fn exact_depth_boundary_is_identical_cold_and_warm() {
    let objects = similar_objects(12, 8 * 1024);
    let written = PackFile::write_packed_with_options(
        &objects,
        ObjectFormat::Sha1,
        &PackWriteOptions::new()
            .with_window(1)
            .with_depth(11)
            .with_reorder(false)
            .with_prefer_ofs_delta(true),
    )
    .expect("write chain");
    let limits = PackReadLimits {
        max_delta_depth: 8,
        ..generous_limits()
    };
    let mut decoder = BoundedPackDecoder::new(
        SlicePackSource::new(&written.pack),
        ObjectFormat::Sha1,
        limits,
    )
    .expect("open decoder");
    let at_limit = written.entries[8].offset;
    let over_limit = written.entries[9].offset;

    let cold = decoder
        .read_object_at(at_limit, |_| Ok(None), |_, _, _| Ok(()))
        .expect("exact depth must pass");
    assert_eq!(cold.stats.delta_depth, 8);
    let warm = decoder
        .read_object_at(at_limit, |_| Ok(None), |_, _, _| Ok(()))
        .expect("warm exact depth must pass");
    assert_eq!(warm.stats.delta_depth, cold.stats.delta_depth);

    for error in [
        decoder
            .read_object_at(over_limit, |_| Ok(None), |_, _, _| Ok(()))
            .expect_err("warm base must not hide full depth"),
        BoundedPackDecoder::new(
            SlicePackSource::new(&written.pack),
            ObjectFormat::Sha1,
            limits,
        )
        .expect("open cold decoder")
        .read_object_at(over_limit, |_| Ok(None), |_, _, _| Ok(()))
        .expect_err("cold over-limit depth must fail"),
    ] {
        assert!(matches!(
            error,
            PackReadError::Limit(limit)
                if limit.kind == PackLimitKind::DeltaDepth && limit.attempted == 9
        ));
    }

    let zero_limits = PackReadLimits {
        max_delta_depth: 0,
        ..generous_limits()
    };
    let mut zero = BoundedPackDecoder::new(
        SlicePackSource::new(&written.pack),
        ObjectFormat::Sha1,
        zero_limits,
    )
    .expect("zero-depth decoder");
    assert_eq!(
        zero.read_object_at(written.entries[0].offset, |_| Ok(None), |_, _, _| Ok(()))
            .expect("undeltified base passes")
            .stats
            .delta_depth,
        0
    );
    assert!(matches!(
        zero.read_object_at(written.entries[1].offset, |_| Ok(None), |_, _, _| Ok(())),
        Err(PackReadError::Limit(limit)) if limit.attempted == 1
    ));
}

#[test]
fn bounded_decoder_preserves_external_ref_delta_resolution() {
    let base = EncodedObject::new(ObjectType::Blob, vec![b'a'; 32 * 1024]);
    let mut target_body = base.body.clone();
    target_body[16 * 1024..16 * 1024 + 8].copy_from_slice(b"changed!");
    let target = EncodedObject::new(ObjectType::Blob, target_body);
    let base_oid = base.object_id(ObjectFormat::Sha1).expect("base id");
    let written = PackFile::write_thin(
        std::slice::from_ref(&target),
        ObjectFormat::Sha1,
        HashMap::from([(base_oid, base.clone())]),
    )
    .expect("write thin pack");
    assert_eq!(written.delta_count, 1, "fixture must use external delta");
    let mut decoder = BoundedPackDecoder::new(
        SlicePackSource::new(&written.pack),
        ObjectFormat::Sha1,
        generous_limits(),
    )
    .expect("open decoder");

    let decoded = decoder
        .read_object_at(
            written.entries[0].offset,
            |oid| {
                Ok((*oid == base_oid).then_some(RefDeltaBase::External {
                    object_type: base.object_type,
                    size: base.body.len(),
                }))
            },
            |oid, out, _| {
                assert_eq!(*oid, base_oid);
                assert_eq!(out.len(), base.body.len());
                out.copy_from_slice(&base.body);
                Ok(())
            },
        )
        .expect("decode thin object");
    assert_eq!(*decoded.object, target);
}

#[test]
fn external_base_budget_rejects_before_fill_is_called() {
    let base = EncodedObject::new(ObjectType::Blob, vec![b'a'; 32 * 1024]);
    let mut target = base.clone();
    target.body[0] = b'b';
    let base_oid = base.object_id(ObjectFormat::Sha1).expect("base id");
    let written = PackFile::write_thin(
        std::slice::from_ref(&target),
        ObjectFormat::Sha1,
        HashMap::from([(base_oid, base)]),
    )
    .expect("thin pack");
    let limits = PackReadLimits {
        max_delta_depth: 8,
        max_materialized_bytes: 16 * 1024,
        max_cached_bytes: 0,
    };
    let mut decoder = BoundedPackDecoder::new(
        SlicePackSource::new(&written.pack),
        ObjectFormat::Sha1,
        limits,
    )
    .expect("decoder");
    let fill_called = std::cell::Cell::new(false);
    let error = decoder
        .read_object_at(
            written.entries[0].offset,
            |_| {
                Ok(Some(RefDeltaBase::External {
                    object_type: ObjectType::Blob,
                    size: 32 * 1024,
                }))
            },
            |_, _, _| {
                fill_called.set(true);
                Ok(())
            },
        )
        .expect_err("external base must be rejected before fill");
    assert!(!fill_called.get());
    assert!(matches!(
        error,
        PackReadError::Limit(limit) if limit.kind == PackLimitKind::MaterializedBytes
    ));
}

#[test]
fn deep_ofs_chain_on_small_stack_returns_typed_depth_limit() {
    let objects = similar_objects(96, 8 * 1024);
    let written = PackFile::write_packed_with_options(
        &objects,
        ObjectFormat::Sha1,
        &PackWriteOptions::new()
            .with_window(1)
            .with_depth(95)
            .with_reorder(false)
            .with_prefer_ofs_delta(true),
    )
    .expect("write deep pack");
    assert!(
        written.delta_count > 32,
        "fixture must contain a deep chain"
    );
    let target_offset = written.entries.last().expect("last entry").offset;

    let error = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let limits = PackReadLimits {
                max_delta_depth: 8,
                ..generous_limits()
            };
            let mut decoder = BoundedPackDecoder::new(
                SlicePackSource::new(&written.pack),
                ObjectFormat::Sha1,
                limits,
            )
            .expect("open decoder");
            decoder
                .read_object_at(target_offset, |_| Ok(None), |_, _, _| Ok(()))
                .expect_err("depth limit must reject chain")
        })
        .expect("spawn small-stack thread")
        .join()
        .expect("small-stack thread must not overflow");

    match error {
        PackReadError::Limit(limit) => {
            assert_eq!(limit.kind, PackLimitKind::DeltaDepth);
            assert_eq!(limit.limit, 8);
            assert_eq!(limit.attempted, 9);
        }
        other => panic!("expected typed depth limit, got {other}"),
    }
}

#[test]
fn declared_body_over_materialized_budget_is_rejected_before_inflate() {
    let object = EncodedObject::new(ObjectType::Blob, vec![b'b'; 32 * 1024]);
    let written = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
        .expect("write pack");
    let limits = PackReadLimits {
        max_delta_depth: 8,
        max_materialized_bytes: 16 * 1024,
        max_cached_bytes: 0,
    };
    let mut decoder = BoundedPackDecoder::new(
        SlicePackSource::new(&written.pack),
        ObjectFormat::Sha1,
        limits,
    )
    .expect("open decoder");

    let error = decoder
        .read_object_at(written.entries[0].offset, |_| Ok(None), |_, _, _| Ok(()))
        .expect_err("body budget must reject object");
    match error {
        PackReadError::Limit(limit) => {
            assert_eq!(limit.kind, PackLimitKind::MaterializedBytes);
            assert_eq!(limit.limit, 16 * 1024);
            assert_eq!(limit.attempted, 32 * 1024);
        }
        other => panic!("expected typed byte limit, got {other}"),
    }
}

#[test]
fn cache_is_byte_bounded_evicts_and_can_be_cleared() {
    let objects = similar_objects(4, 24 * 1024);
    let written = PackFile::write_undeltified(&objects, ObjectFormat::Sha1).expect("write pack");
    let limits = PackReadLimits {
        max_delta_depth: 8,
        max_materialized_bytes: 64 * 1024,
        max_cached_bytes: 30 * 1024,
    };
    let mut decoder = BoundedPackDecoder::new(
        SlicePackSource::new(&written.pack),
        ObjectFormat::Sha1,
        limits,
    )
    .expect("open decoder");
    let mut evictions = 0;

    for entry in &written.entries {
        let decoded = decoder
            .read_object_at(entry.offset, |_| Ok(None), |_, _, _| Ok(()))
            .expect("decode object");
        evictions += decoded.stats.cache_evictions;
        assert!(decoded.stats.cached_bytes <= limits.max_cached_bytes);
        assert!(decoded.stats.peak_materialized_bytes <= limits.max_materialized_bytes);
    }
    assert!(evictions > 0, "cache must evict by byte weight");
    assert_eq!(decoder.cached_objects(), 1);
    decoder.clear_cache();
    assert_eq!(decoder.cached_bytes(), 0);
    assert_eq!(decoder.cached_objects(), 0);
}

#[test]
fn file_source_decodes_positionally_without_whole_pack_residency() {
    let object = EncodedObject::new(ObjectType::Commit, b"tree deadbeef\n\nmessage\n".to_vec());
    let written = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
        .expect("write pack");
    let path = std::env::temp_dir().join(format!(
        "sley-pack-bounded-{}-{}.pack",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::write(&path, &written.pack).expect("write fixture file");
    let file = fs::File::open(&path).expect("open fixture file");
    let mut decoder =
        BoundedPackDecoder::new(file, ObjectFormat::Sha1, generous_limits()).expect("open decoder");

    let decoded = decoder
        .read_object_at(written.entries[0].offset, |_| Ok(None), |_, _, _| Ok(()))
        .expect("decode file-backed object");
    assert_eq!(*decoded.object, object);
    drop(decoder);
    fs::remove_file(path).expect("remove fixture file");
}

#[test]
fn malformed_and_truncated_sources_return_pack_errors() {
    let object = EncodedObject::new(ObjectType::Blob, vec![b'z'; 4096]);
    let written = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
        .expect("write pack");
    let entry_offset = written.entries[0].offset as usize;
    let mut truncated = written.pack[..entry_offset + 3].to_vec();
    truncated.extend_from_slice(&[0; 20]);
    let mut decoder = BoundedPackDecoder::new(
        SlicePackSource::new(&truncated),
        ObjectFormat::Sha1,
        generous_limits(),
    )
    .expect("open truncated source");
    assert!(matches!(
        decoder.read_object_at(entry_offset as u64, |_| Ok(None), |_, _, _| Ok(())),
        Err(PackReadError::Pack(GitError::InvalidObject(_)))
            | Err(PackReadError::Pack(GitError::InvalidFormat(_)))
    ));

    let mut malformed = written.pack.clone();
    malformed[entry_offset] = 0x50;
    let mut decoder = BoundedPackDecoder::new(
        SlicePackSource::new(&malformed),
        ObjectFormat::Sha1,
        generous_limits(),
    )
    .expect("open malformed source");
    assert!(matches!(
        decoder.read_object_at(entry_offset as u64, |_| Ok(None), |_, _, _| Ok(())),
        Err(PackReadError::Pack(GitError::InvalidFormat(_)))
    ));
}

struct CancellingSource<'a> {
    bytes: &'a [u8],
    cancel: &'a AtomicCancel,
    reads: std::cell::Cell<usize>,
}

impl PackReadSource for CancellingSource<'_> {
    fn len(&self) -> std::io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let remaining = self.bytes.get(start..).unwrap_or_default();
        let count = remaining.len().min(buf.len());
        buf[..count].copy_from_slice(&remaining[..count]);
        let reads = self.reads.get() + 1;
        self.reads.set(reads);
        if reads == 1 {
            self.cancel.cancel();
        }
        Ok(count)
    }
}

#[test]
fn cancel_aware_targeted_read_polls_before_and_between_source_reads() {
    let object = EncodedObject::new(ObjectType::Blob, vec![b'q'; 64 * 1024]);
    let written = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1)
        .expect("pack");

    let already = AtomicCancel::new();
    already.cancel();
    let mut decoder = BoundedPackDecoder::new(
        SlicePackSource::new(&written.pack),
        ObjectFormat::Sha1,
        generous_limits(),
    )
    .expect("decoder");
    assert!(matches!(
        decoder.read_object_at_with_cancel(
            written.entries[0].offset,
            |_| Ok(None),
            |_, _, _| Ok(()),
            CancelFlag::new(&already),
        ),
        Err(PackReadError::Pack(GitError::Cancelled))
    ));

    let mid = AtomicCancel::new();
    let source = CancellingSource {
        bytes: &written.pack,
        cancel: &mid,
        reads: std::cell::Cell::new(0),
    };
    let mut decoder =
        BoundedPackDecoder::new(source, ObjectFormat::Sha1, generous_limits()).expect("decoder");
    assert!(matches!(
        decoder.read_object_at_with_cancel(
            written.entries[0].offset,
            |_| Ok(None),
            |_, _, _| Ok(()),
            CancelFlag::new(&mid),
        ),
        Err(PackReadError::Pack(GitError::Cancelled))
    ));
}
