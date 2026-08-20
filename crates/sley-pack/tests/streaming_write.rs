//! Streaming pack writer: known-count iterators and byte-budgeted windows.

use sley_core::{
    AtomicCancel, ByteBudget, CancelFlag, GitError, ObjectFormat, ObjectId, ResourceLimitKind,
    Result,
};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{PackFile, PackWriteLimits, PackWriteOptions};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::rc::Rc;
use std::sync::Arc;

/// One-shot paging iterator: holds at most `page_size` unread ids.
///
/// Does not implement `ExactSizeIterator` or `Clone`.
struct PageIter {
    unread: std::vec::IntoIter<ObjectId>,
    page: Vec<ObjectId>,
    page_size: usize,
}

impl PageIter {
    fn new(ids: Vec<ObjectId>, page_size: usize) -> Self {
        Self {
            unread: ids.into_iter(),
            page: Vec::new(),
            page_size: page_size.max(1),
        }
    }

    fn refill(&mut self) {
        self.page.clear();
        for _ in 0..self.page_size {
            let Some(oid) = self.unread.next() else {
                break;
            };
            self.page.push(oid);
        }
    }
}

impl Iterator for PageIter {
    type Item = ObjectId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.page.is_empty() {
            self.refill();
        }
        if self.page.is_empty() {
            return None;
        }
        Some(self.page.remove(0))
    }
}

struct CountingIter<I> {
    inner: I,
    pulled: Rc<Cell<usize>>,
}

impl<I> Iterator for CountingIter<I>
where
    I: Iterator<Item = ObjectId>,
{
    type Item = ObjectId;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.inner.next()?;
        self.pulled.set(self.pulled.get() + 1);
        Some(next)
    }
}

struct FailAfter {
    remaining: usize,
}

impl Write for FailAfter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::other("forced writer failure"));
        }
        let take = buf.len().min(self.remaining);
        self.remaining -= take;
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn blob_objects(
    format: ObjectFormat,
    count: usize,
    prefix: &[u8],
) -> (Vec<ObjectId>, HashMap<ObjectId, Arc<EncodedObject>>) {
    let mut ids = Vec::with_capacity(count);
    let mut map = HashMap::with_capacity(count);
    for idx in 0..count {
        let mut body = prefix.to_vec();
        body.extend_from_slice(format!(" {idx}\n").as_bytes());
        let object = EncodedObject::new(ObjectType::Blob, body);
        let oid = object.object_id(format).expect("oid");
        ids.push(oid);
        map.insert(oid, Arc::new(object));
    }
    (ids, map)
}

fn write_from_map(
    ids: impl IntoIterator<Item = ObjectId>,
    object_count: u32,
    format: ObjectFormat,
    options: &PackWriteOptions,
    limits: PackWriteLimits,
    map: &HashMap<ObjectId, Arc<EncodedObject>>,
) -> Result<Vec<u8>> {
    let mut written = Vec::new();
    PackFile::write_packed_from_source_to_writer(
        ids,
        object_count,
        format,
        options,
        limits,
        |oid| {
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
    )?;
    Ok(written)
}

#[test]
fn paging_iterator_writes_valid_sha1_and_sha256_packs() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let (ids, map) = blob_objects(format, 17, b"paged blob");
        let page = PageIter::new(ids, 4);
        let pack = write_from_map(
            page,
            17,
            format,
            &PackWriteOptions::new().with_reorder(false),
            PackWriteLimits::default(),
            &map,
        )
        .expect("paged write");
        let parsed = PackFile::parse(&pack, format).expect("parse paged pack");
        assert_eq!(parsed.entries.len(), 17);
        assert_eq!(parsed.checksum.format(), format);
        PackFile::verify_pack_stats(&pack, format).expect("verify paged pack");
    }
}

#[test]
fn count_too_few_is_typed_error_and_not_success() {
    let format = ObjectFormat::Sha1;
    let (ids, map) = blob_objects(format, 3, b"short");
    let err = write_from_map(
        ids,
        5,
        format,
        &PackWriteOptions::new(),
        PackWriteLimits::default(),
        &map,
    )
    .expect_err("short iterator must fail");
    assert_eq!(
        err,
        GitError::CountMismatch {
            expected: 5,
            actual: 3
        }
    );
}

#[test]
fn count_too_many_is_typed_error_before_extra_source_read() {
    let format = ObjectFormat::Sha1;
    let (ids, map) = blob_objects(format, 4, b"long");
    let reads = Rc::new(Cell::new(0usize));
    let pulled = Rc::new(Cell::new(0usize));
    let mut written = Vec::new();
    let err = PackFile::write_packed_from_source_to_writer(
        CountingIter {
            inner: ids.into_iter(),
            pulled: Rc::clone(&pulled),
        },
        2,
        format,
        &PackWriteOptions::new(),
        PackWriteLimits::default(),
        |oid| {
            reads.set(reads.get() + 1);
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
    )
    .expect_err("overlong iterator must fail");
    assert_eq!(
        err,
        GitError::CountMismatch {
            expected: 2,
            actual: 3
        }
    );
    assert_eq!(reads.get(), 2, "must not read the extra object from source");
    assert_eq!(
        pulled.get(),
        3,
        "must observe the extra id to detect overlong"
    );
}

#[test]
fn writer_failure_stops_enumeration() {
    let format = ObjectFormat::Sha1;
    let (ids, map) = blob_objects(format, 8, b"backpressure");
    let pulled = Rc::new(Cell::new(0usize));
    let reads = Rc::new(Cell::new(0usize));
    let limits = PackWriteLimits::new().with_compression_working_set(ByteBudget::new(80));
    let mut writer = FailAfter { remaining: 16 };
    let err = PackFile::write_packed_from_source_to_writer(
        CountingIter {
            inner: ids.into_iter(),
            pulled: Rc::clone(&pulled),
        },
        8,
        format,
        &PackWriteOptions::new().with_reorder(false),
        limits,
        |oid| {
            reads.set(reads.get() + 1);
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut writer,
    )
    .expect_err("writer failure must fail the pack");
    assert!(matches!(err, GitError::Io(_)), "got {err:?}");
    assert!(
        pulled.get() < 8,
        "writer failure must stop id enumeration, pulled {}",
        pulled.get()
    );
    assert!(
        reads.get() < 8,
        "writer failure must stop source reads, reads {}",
        reads.get()
    );
}

#[test]
fn source_failure_stops_enumeration() {
    let format = ObjectFormat::Sha1;
    let (ids, map) = blob_objects(format, 6, b"source fail");
    let pulled = Rc::new(Cell::new(0usize));
    let reads = Rc::new(Cell::new(0usize));
    let mut written = Vec::new();
    let err = PackFile::write_packed_from_source_to_writer(
        CountingIter {
            inner: ids.into_iter(),
            pulled: Rc::clone(&pulled),
        },
        6,
        format,
        &PackWriteOptions::new(),
        PackWriteLimits::default(),
        |oid| {
            let n = reads.get() + 1;
            reads.set(n);
            if n == 2 {
                return Err(GitError::not_found("injected source failure"));
            }
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
    )
    .expect_err("source failure must fail the pack");
    assert!(matches!(err, GitError::NotFound(_)), "got {err:?}");
    assert_eq!(reads.get(), 2);
    assert!(
        pulled.get() <= 3,
        "must not drain the iterator after source failure, pulled {}",
        pulled.get()
    );
}

#[test]
fn oversized_object_is_one_object_quantum() {
    let format = ObjectFormat::Sha1;
    let small = EncodedObject::new(ObjectType::Blob, b"tiny\n".to_vec());
    let large = EncodedObject::new(ObjectType::Blob, vec![b'L'; 400]);
    let small_oid = small.object_id(format).expect("oid");
    let large_oid = large.object_id(format).expect("oid");
    let map = HashMap::from([(small_oid, Arc::new(small)), (large_oid, Arc::new(large))]);
    let limits = PackWriteLimits::new()
        .with_compression_working_set(ByteBudget::new(80))
        .with_decoded_object(ByteBudget::new(1024));
    let pack = write_from_map(
        [small_oid, large_oid],
        2,
        format,
        &PackWriteOptions::new().with_reorder(false),
        limits,
        &map,
    )
    .expect("one-object quantum");
    let parsed = PackFile::parse(&pack, format).expect("parse");
    assert_eq!(parsed.entries.len(), 2);
}

#[test]
fn oversized_object_above_decoded_limit_is_typed_error() {
    let format = ObjectFormat::Sha1;
    let large = EncodedObject::new(ObjectType::Blob, vec![b'X'; 200]);
    let oid = large.object_id(format).expect("oid");
    let map = HashMap::from([(oid, Arc::new(large))]);
    let limits = PackWriteLimits::new()
        .with_compression_working_set(ByteBudget::new(64))
        .with_decoded_object(ByteBudget::new(50));
    let err = write_from_map([oid], 1, format, &PackWriteOptions::new(), limits, &map)
        .expect_err("decoded limit must reject");
    assert_eq!(
        err,
        GitError::ResourceLimit {
            kind: ResourceLimitKind::DecodedObject,
            limit: 50,
            attempted: 200,
        }
    );
}

#[test]
fn peak_working_set_stays_within_budgets_plus_one_object() {
    let format = ObjectFormat::Sha1;
    let (ids, map) = blob_objects(format, 12, b"budgeted");
    let working = ByteBudget::new(180);
    let bases = ByteBudget::new(120);
    let limits = PackWriteLimits::new()
        .with_compression_working_set(working)
        .with_delta_base(bases)
        .with_decoded_object(ByteBudget::new(1024));
    let mut written = Vec::new();
    let summary = PackFile::write_packed_from_source_to_writer(
        ids.iter().copied(),
        12,
        format,
        &PackWriteOptions::new().with_reorder(false),
        limits,
        |oid| {
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
    )
    .expect("budgeted write");
    let ceiling = working
        .as_u64()
        .saturating_add(bases.as_u64())
        .saturating_add(180);
    assert!(
        summary.peak_working_set_bytes <= ceiling,
        "peak {} exceeded documented ceiling {ceiling}",
        summary.peak_working_set_bytes
    );
    PackFile::verify_pack_stats(&written, format).expect("verify");
}

#[test]
fn empty_known_count_writes_empty_pack() {
    let format = ObjectFormat::Sha1;
    let pack = write_from_map(
        std::iter::empty(),
        0,
        format,
        &PackWriteOptions::new(),
        PackWriteLimits::default(),
        &HashMap::new(),
    )
    .expect("empty pack");
    let parsed = PackFile::parse(&pack, format).expect("parse empty");
    assert!(parsed.entries.is_empty());
}

#[test]
fn huge_declared_count_with_empty_iterator_is_count_mismatch() {
    let format = ObjectFormat::Sha1;
    let err = write_from_map(
        std::iter::empty(),
        u32::MAX,
        format,
        &PackWriteOptions::new(),
        PackWriteLimits::default(),
        &HashMap::new(),
    )
    .expect_err("unverified huge count must not allocate a successful pack");
    assert_eq!(
        err,
        GitError::CountMismatch {
            expected: u64::from(u32::MAX),
            actual: 0
        }
    );
}

#[test]
fn leftover_lookahead_is_charged_to_peak_working_set() {
    let format = ObjectFormat::Sha1;
    let first = EncodedObject::new(ObjectType::Blob, vec![b'A'; 100]);
    let second = EncodedObject::new(ObjectType::Blob, vec![b'B'; 100]);
    let first_oid = first.object_id(format).expect("oid");
    let second_oid = second.object_id(format).expect("oid");
    let map = HashMap::from([(first_oid, Arc::new(first)), (second_oid, Arc::new(second))]);
    let object_cost = 100u64 + 64;
    let limits = PackWriteLimits::new()
        .with_compression_working_set(ByteBudget::new(200))
        .with_delta_base(ByteBudget::ZERO)
        .with_decoded_object(ByteBudget::new(1024));
    let mut written = Vec::new();
    let summary = PackFile::write_packed_from_source_to_writer(
        [first_oid, second_oid],
        2,
        format,
        &PackWriteOptions::new().with_depth(0).with_reorder(false),
        limits,
        |oid| {
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
    )
    .expect("leftover write");
    assert_eq!(
        summary.peak_working_set_bytes,
        object_cost * 2,
        "peak must include the decoded leftover lookahead"
    );
    PackFile::verify_pack_stats(&written, format).expect("verify");
}

#[test]
fn duplicate_ids_are_rejected_before_self_ref_delta() {
    let format = ObjectFormat::Sha1;
    let object = EncodedObject::new(ObjectType::Blob, b"dup\n".to_vec());
    let oid = object.object_id(format).expect("oid");
    let map = HashMap::from([(oid, Arc::new(object))]);
    let err = write_from_map(
        [oid, oid],
        2,
        format,
        &PackWriteOptions::new().with_prefer_ofs_delta(false),
        PackWriteLimits::default(),
        &map,
    )
    .expect_err("duplicate ids must fail");
    assert!(
        matches!(err, GitError::InvalidFormat(ref msg) if msg.contains("duplicate object id")),
        "got {err:?}"
    );
}

#[test]
fn cancel_is_polled_while_filling_tiny_object_window() {
    let format = ObjectFormat::Sha1;
    let (ids, map) = blob_objects(format, 64, b"");
    let reads = Rc::new(Cell::new(0usize));
    let source = AtomicCancel::new();
    let mut written = Vec::new();
    let err = PackFile::write_packed_from_source_to_writer_with_cancel(
        ids,
        64,
        format,
        &PackWriteOptions::new().with_reorder(false),
        PackWriteLimits::default(),
        |oid| {
            let n = reads.get() + 1;
            reads.set(n);
            if n == 1 {
                source.cancel();
            }
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
        CancelFlag::new(&source),
    )
    .expect_err("cancel during fill must fail");
    assert_eq!(err, GitError::Cancelled);
    assert_eq!(
        reads.get(),
        1,
        "must not keep filling a tiny-object window after cancel"
    );
}

#[test]
fn empty_delta_bases_charge_metadata_against_horizon() {
    let format = ObjectFormat::Sha1;
    let (ids, map) = blob_objects(format, 8, b"");
    let limits = PackWriteLimits::new()
        .with_compression_working_set(ByteBudget::new(8 * 64))
        .with_delta_base(ByteBudget::new(64 * 2))
        .with_decoded_object(ByteBudget::new(1024));
    let mut written = Vec::new();
    let summary = PackFile::write_packed_from_source_to_writer(
        ids,
        8,
        format,
        &PackWriteOptions::new().with_window(32).with_reorder(false),
        limits,
        |oid| {
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
    )
    .expect("empty-base write");
    assert!(
        summary.peak_working_set_bytes >= 64 * 2,
        "empty retained bases must still charge metadata, peak {}",
        summary.peak_working_set_bytes
    );
    let unpaid_horizon = 8u64 * 64;
    assert!(
        summary.peak_working_set_bytes < unpaid_horizon.saturating_add(8 * 64),
        "horizon must evict empty bases once metadata exceeds delta_base, peak {}",
        summary.peak_working_set_bytes
    );
}

#[test]
fn undeltified_source_path_round_trips() {
    let format = ObjectFormat::Sha1;
    let (ids, map) = blob_objects(format, 5, b"undeltified stream");
    let mut written = Vec::new();
    let summary = PackFile::write_undeltified_from_source_to_writer(
        ids.iter().copied(),
        5,
        format,
        &PackWriteOptions::new().with_depth(0),
        PackWriteLimits::default(),
        |oid| {
            map.get(oid)
                .cloned()
                .ok_or_else(|| GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
    )
    .expect("undeltified stream");
    assert_eq!(summary.delta_count, 0);
    let parsed = PackFile::parse(&written, format).expect("parse");
    assert_eq!(parsed.entries.len(), 5);
}
