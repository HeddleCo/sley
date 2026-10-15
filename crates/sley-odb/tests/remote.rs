use sley_core::{GitError, ObjectFormat};
use sley_object::{EncodedObject, ObjectType};
use sley_odb::{
    MemoryRemoteBackend, RemoteBufferPool, RemoteObjectDatabase, RemoteOdbBackend, RemoteOdbLimits,
    RemotePackLocation,
};
use sley_pack::{PackDecodeBudget, PackFile, PackReadLimits, PackWrite, PackWriteOptions};
use std::{
    ops::Range,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Probe {
    inner: MemoryRemoteBackend,
    active: AtomicUsize,
    peak: AtomicUsize,
    reads: Mutex<Vec<(String, Range<u64>)>>,
    short: AtomicBool,
}
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl RemoteOdbBackend for Probe {
    async fn read_range(
        &self,
        key: &str,
        range: Range<u64>,
        output: &mut [u8],
    ) -> sley_core::Result<usize> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = Active(&self.active);
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.reads
            .lock()
            .expect("reads")
            .push((key.into(), range.clone()));
        tokio::time::sleep(Duration::from_millis(1)).await;
        let count = self.inner.read_range(key, range, output).await?;
        Ok(if self.short.load(Ordering::Relaxed) {
            count.saturating_sub(1)
        } else {
            count
        })
    }
}
fn probe(written: &PackWrite) -> Arc<Probe> {
    Arc::new(Probe {
        inner: MemoryRemoteBackend::new([
            ("pack".into(), Arc::from(written.pack.clone())),
            ("index".into(), Arc::from(written.index.clone())),
        ]),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        reads: Mutex::new(Vec::new()),
        short: AtomicBool::new(false),
    })
}
fn location(written: &PackWrite) -> RemotePackLocation {
    RemotePackLocation {
        pack_key: "pack".into(),
        index_key: "index".into(),
        pack_len: written.pack.len() as u64,
        index_len: written.index.len() as u64,
        format: written.checksum.format(),
    }
}
fn limits() -> RemoteOdbLimits {
    RemoteOdbLimits {
        max_index_bytes: 1024 * 1024,
        max_encoded_bytes: 1024 * 1024,
        decode: PackReadLimits {
            max_delta_depth: 4095,
            max_materialized_bytes: 1024 * 1024,
            max_cached_bytes: 0,
        },
    }
}
fn objects(count: u64) -> Vec<EncodedObject> {
    (0..count)
        .map(|n| {
            let mut body: Vec<_> = (0..8192u64)
                .map(|i| ((i.wrapping_mul(6364136223846793005) >> (i % 32)) & 255) as u8)
                .collect();
            body[8184..].copy_from_slice(&n.to_be_bytes());
            EncodedObject::new(ObjectType::Blob, body)
        })
        .collect()
}
fn pack(objects: &[EncodedObject], format: ObjectFormat, ofs: bool) -> PackWrite {
    PackFile::write_packed_with_options(
        objects,
        format,
        &PackWriteOptions::new()
            .with_reorder(false)
            .with_window(1)
            .with_depth(4095)
            .with_prefer_ofs_delta(ofs),
    )
    .expect("pack")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ranged_odb_matches_original_bytes_and_ranges_overlap() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        for ofs in [false, true] {
            let objects = objects(25);
            let written = pack(&objects, format, ofs);
            assert!(written.delta_count > 0);
            let backend = probe(&written);
            let pool = RemoteBufferPool::new(1024, 4).expect("pool");
            let budget = Arc::new(PackDecodeBudget::new(16 * 1024 * 1024));
            let db = RemoteObjectDatabase::open(
                backend.clone(),
                pool.clone(),
                budget.clone(),
                location(&written),
                limits(),
            )
            .await
            .expect("open");
            let ids: Vec<_> = [24, 0, 12]
                .into_iter()
                .map(|i| objects[i].object_id(format).expect("oid"))
                .collect();
            let actual = db.read_objects(&ids).await.expect("read objects");
            for (actual, i) in actual.iter().zip([24, 0, 12]) {
                assert_eq!(actual.as_ref(), &objects[i]);
            }
            assert!(
                backend.peak.load(Ordering::SeqCst) > 1,
                "ranges overlap in flight"
            );
            assert!(backend.peak.load(Ordering::SeqCst) <= 4);
            assert_eq!(pool.available_buffers(), 4);
            assert_eq!(budget.used_bytes(), 0);
            backend.reads.lock().expect("reads").clear();
            assert_eq!(
                db.read_object(&ids[1]).await.expect("root").as_ref(),
                &objects[0]
            );
            let reads = backend.reads.lock().expect("reads");
            let total: u64 = reads
                .iter()
                .filter(|(key, _)| key == "pack")
                .map(|(_, r)| r.end - r.start)
                .sum();
            assert!(
                total < written.pack.len() as u64,
                "lookup does not fetch whole pack"
            );
            assert!(
                reads
                    .iter()
                    .all(|(_, range)| range.end - range.start <= 1024)
            );
        }
    }
}

#[tokio::test]
async fn short_read_and_corruption_fail_and_release_pool() {
    let objects = objects(4);
    let mut written = pack(&objects, ObjectFormat::Sha1, true);
    let oid = objects[0].object_id(ObjectFormat::Sha1).expect("oid");
    let backend = probe(&written);
    let pool = RemoteBufferPool::new(1024, 2).expect("pool");
    let budget = Arc::new(PackDecodeBudget::new(4 * 1024 * 1024));
    let db = RemoteObjectDatabase::open(
        backend.clone(),
        pool.clone(),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open");
    backend.short.store(true, Ordering::Relaxed);
    let error = db.read_object(&oid).await.expect_err("short read rejected");
    assert!(error.to_string().contains("short remote range"));
    assert_eq!(pool.available_buffers(), 2);
    assert_eq!(budget.used_bytes(), 0);
    written.pack[20] ^= 1;
    let db = RemoteObjectDatabase::open(
        probe(&written),
        pool.clone(),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open headers");
    assert!(
        db.read_object(&oid)
            .await
            .expect_err("CRC rejected")
            .to_string()
            .contains("CRC mismatch")
    );
    assert_eq!(pool.available_buffers(), 2);
    assert_eq!(budget.used_bytes(), 0);
}

#[tokio::test]
async fn missing_base_budget_and_index_limit_are_rejected() {
    let objects = objects(2);
    let format = ObjectFormat::Sha1;
    let base = objects[0].object_id(format).expect("base oid");
    let written = PackFile::write_packed_with_options(
        &objects[1..],
        format,
        &PackWriteOptions::new()
            .with_reorder(false)
            .with_thin_bases([(base, objects[0].clone())].into()),
    )
    .expect("thin");
    assert_eq!(written.delta_count, 1);
    let pool = RemoteBufferPool::new(1024, 2).expect("pool");
    let budget = Arc::new(PackDecodeBudget::new(4 * 1024 * 1024));
    let db = RemoteObjectDatabase::open(
        probe(&written),
        pool.clone(),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open");
    assert!(matches!(
        db.read_object(&objects[1].object_id(format).expect("oid"))
            .await,
        Err(GitError::NotFound(_))
    ));
    let mut small = limits();
    small.max_index_bytes = 1;
    assert!(
        RemoteObjectDatabase::open(
            probe(&written),
            pool.clone(),
            budget.clone(),
            location(&written),
            small
        )
        .await
        .is_err()
    );
    assert_eq!(budget.used_bytes(), 0);
    let written = pack(&objects, format, true);
    let budget = Arc::new(PackDecodeBudget::new(1));
    let db = RemoteObjectDatabase::open(
        probe(&written),
        pool.clone(),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open");
    assert!(
        db.read_object(&objects[0].object_id(format).expect("oid"))
            .await
            .expect_err("budget")
            .to_string()
            .contains("limit exceeded")
    );
    assert_eq!(budget.used_bytes(), 0);
    assert_eq!(pool.available_buffers(), 2);
}

#[tokio::test]
async fn cancellation_returns_buffers_and_deep_future_is_small() {
    let objects = objects(129);
    let written = pack(&objects, ObjectFormat::Sha1, false);
    let backend = probe(&written);
    let pool = RemoteBufferPool::new(1024, 4).expect("pool");
    let budget = Arc::new(PackDecodeBudget::new(4 * 1024 * 1024));
    let db = RemoteObjectDatabase::open(
        backend.clone(),
        pool.clone(),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open");
    let oid = objects[128].object_id(ObjectFormat::Sha1).expect("oid");
    let read = db.read_object(&oid);
    assert!(std::mem::size_of_val(&read) < 8192);
    {
        tokio::pin!(read);
        tokio::select! {
            result = &mut read => panic!("read completed before cancellation: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
    }
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
    assert_eq!(pool.available_buffers(), 4);
    assert_eq!(budget.used_bytes(), 0);
    assert_eq!(
        db.read_object(&oid).await.expect("deep chain").as_ref(),
        &objects[128]
    );
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn invalid_pool_dimensions_are_rejected() {
    assert!(RemoteBufferPool::new(0, 1).is_err());
    assert!(RemoteBufferPool::new(1, 0).is_err());
    assert!(RemoteBufferPool::new(usize::MAX, 2).is_err());
}

#[tokio::test]
async fn valid_crc_with_wrong_index_identity_is_rejected() {
    let objects = objects(2);
    let format = ObjectFormat::Sha1;
    let mut written = pack(&objects, format, true);
    let original = objects[0].object_id(format).expect("original oid");
    let wrong = EncodedObject::new(ObjectType::Blob, b"wrong indexed body".to_vec())
        .object_id(format)
        .expect("wrong oid");
    let mut index = sley_pack::PackIndex::parse(&written.index, format).expect("index");
    index
        .entries
        .iter_mut()
        .find(|entry| entry.oid == original)
        .expect("root entry")
        .oid = wrong;
    written.index = sley_pack::PackIndex::write_v2(format, &index.entries, &index.pack_checksum)
        .expect("index bytes");
    let pool = RemoteBufferPool::new(1024, 2).expect("pool");
    let budget = Arc::new(PackDecodeBudget::new(4 * 1024 * 1024));
    let db = RemoteObjectDatabase::open(
        probe(&written),
        pool.clone(),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open");
    assert!(
        db.read_object(&wrong)
            .await
            .expect_err("identity checked after decoding")
            .to_string()
            .contains("identity mismatch")
    );
    assert_eq!(pool.available_buffers(), 2);
    assert_eq!(budget.used_bytes(), 0);
}
