use sley_core::{AtomicCancel, CancelFlag, GitError, ObjectFormat, ObjectId};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{
    BoundedPackDecoder, PackFile, PackLimitKind, PackObjectLocation, PackReadError, PackReadLimits,
    PackReadSource, PackWriteOptions, RefDeltaBases,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn limits(depth: usize, materialized: usize, cached: usize) -> PackReadLimits {
    PackReadLimits {
        max_delta_depth: depth,
        max_materialized_bytes: materialized,
        max_cached_bytes: cached,
    }
}

struct MultiPackChain {
    packs: Vec<Vec<u8>>,
    offsets: Vec<u64>,
    oids: Vec<ObjectId>,
    objects: Vec<EncodedObject>,
}

fn multi_pack_chain(depth: usize, body_len: usize, format: ObjectFormat) -> MultiPackChain {
    let mut objects = Vec::with_capacity(depth + 1);
    let mut packs = Vec::with_capacity(depth + 1);
    let mut offsets = Vec::with_capacity(depth + 1);
    let mut oids = Vec::with_capacity(depth + 1);

    let base = EncodedObject::new(ObjectType::Blob, vec![b'x'; body_len]);
    let written = PackFile::write_undeltified(std::slice::from_ref(&base), format).expect("base");
    offsets.push(written.entries[0].offset);
    oids.push(base.object_id(format).expect("base id"));
    packs.push(written.pack);
    objects.push(base);

    for index in 1..=depth {
        let previous = objects.last().expect("previous").clone();
        let previous_oid = oids[index - 1];
        let mut next = previous.clone();
        next.body[body_len - 8..].copy_from_slice(&(index as u64).to_be_bytes());
        let written = PackFile::write_thin(
            std::slice::from_ref(&next),
            format,
            HashMap::from([(previous_oid, previous)]),
        )
        .expect("thin pack");
        assert_eq!(
            written.delta_count, 1,
            "fixture link {index} must be a delta"
        );
        offsets.push(written.entries[0].offset);
        oids.push(next.object_id(format).expect("object id"));
        packs.push(written.pack);
        objects.push(next);
    }
    MultiPackChain {
        packs,
        offsets,
        oids,
        objects,
    }
}

fn open_chain(
    chain: &MultiPackChain,
    read_limits: PackReadLimits,
) -> (
    BoundedPackDecoder<Vec<u8>>,
    RefDeltaBases,
    Vec<PackObjectLocation>,
) {
    let mut decoder =
        BoundedPackDecoder::new(chain.packs[0].clone(), ObjectFormat::Sha1, read_limits)
            .expect("decoder");
    let mut source_ids = vec![decoder.primary_source()];
    for pack in chain.packs.iter().skip(1) {
        source_ids.push(
            decoder
                .add_source(pack.clone(), ObjectFormat::Sha1)
                .expect("add source"),
        );
    }
    let locations: Vec<_> = source_ids
        .into_iter()
        .zip(&chain.offsets)
        .map(|(source, offset)| PackObjectLocation::new(source, *offset))
        .collect();
    let mut bases = RefDeltaBases::new();
    for (oid, location) in chain.oids.iter().copied().zip(locations.iter().copied()) {
        bases.insert_location(oid, location);
    }
    (decoder, bases, locations)
}

#[test]
fn immutable_resolved_base_is_identity_bound_and_counted_without_copying() {
    let chain = multi_pack_chain(0, 24 * 1024, ObjectFormat::Sha1);
    let (mut base_decoder, empty, base_locations) = open_chain(&chain, limits(8, 128 * 1024, 0));
    let base_outcome = base_decoder
        .read_object_at_location(base_locations[0], &empty)
        .expect("base outcome");
    let resolved = base_outcome.resolved_base();

    let base = chain.objects[0].clone();
    let base_oid = chain.oids[0];
    let mut target = base.clone();
    target.body[0] = b'y';
    let thin = PackFile::write_thin(
        std::slice::from_ref(&target),
        ObjectFormat::Sha1,
        HashMap::from([(base_oid, base)]),
    )
    .expect("thin");
    let mut bases = RefDeltaBases::new();
    bases
        .insert_resolved(base_oid, resolved.clone())
        .expect("matching identity");
    assert!(
        bases
            .insert_resolved(
                target.object_id(ObjectFormat::Sha1).expect("target id"),
                resolved
            )
            .is_err(),
        "safe code cannot bind one resolved object to another identity"
    );

    let mut generous = BoundedPackDecoder::new(
        thin.pack.clone(),
        ObjectFormat::Sha1,
        limits(8, 128 * 1024, 0),
    )
    .expect("generous");
    let measured = generous
        .read_object_at(thin.entries[0].offset, &bases)
        .expect("resolved external base");
    assert_eq!(measured.object(), &target);
    assert!(
        measured.stats().peak_materialized_bytes() >= target.body.len() * 2,
        "the live immutable base and result must both participate in the budget"
    );
    let exact_peak = measured.stats().peak_materialized_bytes();

    let mut exact = BoundedPackDecoder::new(
        thin.pack.clone(),
        ObjectFormat::Sha1,
        limits(8, exact_peak, 0),
    )
    .expect("exact");
    exact
        .read_object_at(thin.entries[0].offset, &bases)
        .expect("exact shared budget passes");
    let mut under =
        BoundedPackDecoder::new(thin.pack, ObjectFormat::Sha1, limits(8, exact_peak - 1, 0))
            .expect("under");
    assert!(matches!(
        under.read_object_at(thin.entries[0].offset, &bases),
        Err(PackReadError::Limit(limit)) if limit.kind == PackLimitKind::MaterializedBytes
    ));
}

#[test]
fn cold_multi_pack_chain_is_iterative_and_enforces_cumulative_depth() {
    const DEPTH: usize = 192;
    let chain = multi_pack_chain(DEPTH, 4096, ObjectFormat::Sha1);
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let (mut decoder, bases, locations) =
                open_chain(&chain, limits(DEPTH, 64 * 1024, 8 * 1024));
            let decoded = decoder
                .read_object_at_location(locations[DEPTH], &bases)
                .expect("deep cold cross-pack chain");
            assert_eq!(decoded.object(), &chain.objects[DEPTH]);
            assert_eq!(decoded.stats().delta_depth(), DEPTH);

            let (mut over, bases, locations) =
                open_chain(&chain, limits(DEPTH - 1, 64 * 1024, 8 * 1024));
            assert!(matches!(
                over.read_object_at_location(locations[DEPTH], &bases),
                Err(PackReadError::Limit(limit))
                    if limit.kind == PackLimitKind::DeltaDepth
                        && limit.attempted == DEPTH
            ));
        })
        .expect("small-stack thread")
        .join()
        .expect("iterative decoder must not overflow");
}

#[derive(Clone)]
struct CancellingOwnedSource {
    bytes: Vec<u8>,
    reads: Arc<AtomicUsize>,
    cancel_after: Arc<AtomicUsize>,
    cancel: Arc<AtomicCancel>,
}

impl PackReadSource for CancellingOwnedSource {
    fn len(&self) -> std::io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let remaining = self.bytes.get(start..).unwrap_or_default();
        let count = remaining.len().min(buf.len());
        buf[..count].copy_from_slice(&remaining[..count]);
        let reads = self.reads.fetch_add(1, Ordering::AcqRel) + 1;
        if reads >= self.cancel_after.load(Ordering::Acquire) {
            self.cancel.cancel();
        }
        Ok(count)
    }
}

#[test]
fn cancelled_deep_multi_pack_read_does_not_poison_cache() {
    let chain = multi_pack_chain(48, 4096, ObjectFormat::Sha1);
    let reads = Arc::new(AtomicUsize::new(0));
    let cancel_after = Arc::new(AtomicUsize::new(24));
    let cancel = Arc::new(AtomicCancel::new());
    let wrap = |bytes: Vec<u8>| CancellingOwnedSource {
        bytes,
        reads: Arc::clone(&reads),
        cancel_after: Arc::clone(&cancel_after),
        cancel: Arc::clone(&cancel),
    };
    let mut decoder = BoundedPackDecoder::new(
        wrap(chain.packs[0].clone()),
        ObjectFormat::Sha1,
        limits(64, 64 * 1024, 32 * 1024),
    )
    .expect("decoder");
    let mut source_ids = vec![decoder.primary_source()];
    for pack in chain.packs.iter().skip(1) {
        source_ids.push(
            decoder
                .add_source(wrap(pack.clone()), ObjectFormat::Sha1)
                .expect("source"),
        );
    }
    let locations: Vec<_> = source_ids
        .into_iter()
        .zip(&chain.offsets)
        .map(|(source, offset)| PackObjectLocation::new(source, *offset))
        .collect();
    let mut bases = RefDeltaBases::new();
    for (oid, location) in chain.oids.iter().copied().zip(locations.iter().copied()) {
        bases.insert_location(oid, location);
    }

    assert!(matches!(
        decoder.read_object_at_location_with_cancel(
            locations[48],
            &bases,
            CancelFlag::new(&cancel),
        ),
        Err(PackReadError::Pack(GitError::Cancelled))
    ));
    assert_eq!(decoder.cached_objects(), 0, "cancelled work is not cached");

    cancel.clear();
    cancel_after.store(usize::MAX, Ordering::Release);
    let decoded = decoder
        .read_object_at_location_with_cancel(locations[48], &bases, CancelFlag::new(&cancel))
        .expect("retry after cancellation");
    assert_eq!(decoded.object(), &chain.objects[48]);
}

#[test]
fn exact_compressed_stats_exclude_prefixes_and_zlib_read_ahead() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        for prefer_ofs_delta in [true, false] {
            let mut objects = vec![
                EncodedObject::new(ObjectType::Blob, vec![b'a'; 16 * 1024]),
                EncodedObject::new(ObjectType::Blob, vec![b'a'; 16 * 1024]),
            ];
            objects[1].body[0] = b'b';
            let mut written = PackFile::write_packed_with_options(
                &objects,
                format,
                &PackWriteOptions::new()
                    .with_window(1)
                    .with_depth(1)
                    .with_reorder(false)
                    .with_prefer_ofs_delta(prefer_ofs_delta),
            )
            .expect("pack");
            assert_eq!(written.delta_count, 1);
            let trailer_len = format.raw_len();
            let trailer = written.pack.split_off(written.pack.len() - trailer_len);
            written.pack.extend_from_slice(&[0xa5; 257]);
            written.pack.extend_from_slice(&trailer);

            let mut decoder =
                BoundedPackDecoder::new(written.pack, format, limits(8, 64 * 1024, 0))
                    .expect("decoder");
            let mut bases = RefDeltaBases::new();
            for entry in &written.entries {
                bases.insert_location(
                    entry.oid,
                    PackObjectLocation::new(decoder.primary_source(), entry.offset),
                );
            }
            let decoded = decoder
                .read_object_at(written.entries[1].offset, &bases)
                .expect("delta target");
            let expected: u64 = written
                .entries
                .iter()
                .map(|entry| entry.compressed_size)
                .sum();
            assert_eq!(decoded.stats().compressed_bytes_read(), expected);
            assert!(decoded.stats().source_bytes_read() > expected);
        }
    }
}
