#![cfg(feature = "remote-odb")]

use sley_core::{GitError, ObjectFormat};
use sley_object::{EncodedObject, ObjectType};
use sley_odb::{
    MemoryRemoteBackend, RemoteBufferPool, RemoteObjectDatabase, RemoteOdbBackend, RemoteOdbError,
    RemoteOdbLimits, RemotePackLocation,
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
        Err(RemoteOdbError::Git(GitError::NotFound(_)))
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
    assert!(matches!(
        db.read_object(&objects[0].object_id(format).expect("oid")).await,
        Err(RemoteOdbError::Decode(sley_pack::PackReadError::Limit(error)))
            if error.kind == sley_pack::PackLimitKind::MaterializedBytes && error.limit == 1
    ));
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
    assert!(RemoteBufferPool::with_cpu_workers(1024, 4, 0).is_err());
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

#[test]
fn completed_cpu_result_keeps_budget_until_async_delivery_or_drop() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .expect("runtime");
    let objects = objects(1);
    let format = ObjectFormat::Sha1;
    let written = pack(&objects, format, true);
    let backend = Arc::new(MemoryRemoteBackend::new([
        ("pack".into(), Arc::from(written.pack.clone())),
        ("index".into(), Arc::from(written.index.clone())),
    ]));
    let pool = RemoteBufferPool::new(1024, 1).expect("pool");
    let budget = Arc::new(PackDecodeBudget::new(4 * 1024 * 1024));
    let db = runtime
        .block_on(RemoteObjectDatabase::open(
            backend,
            pool,
            budget.clone(),
            location(&written),
            limits(),
        ))
        .expect("open");
    let oid = objects[0].object_id(format).expect("oid");
    let (release, wait) = std::sync::mpsc::sync_channel(0);
    let blocker = runtime.spawn_blocking(move || wait.recv().expect("release blocker"));
    let retained = {
        let mut read = std::pin::pin!(db.read_object(&oid));
        {
            let _entered = runtime.enter();
            let mut context = Context::from_waker(Waker::noop());
            assert!(matches!(read.as_mut().poll(&mut context), Poll::Pending));
        }
        assert!(budget.used_bytes() > 0, "queued CPU job owns its charge");
        release.send(()).expect("release CPU worker");
        runtime.block_on(blocker).expect("blocker");
        // With one blocking thread this fence runs after the decode job, while
        // the read future remains unpolled and its completed result is retained.
        runtime
            .block_on(runtime.spawn_blocking(|| ()))
            .expect("CPU fence");
        budget.used_bytes()
    };
    assert!(
        retained > 0,
        "completed but undelivered result stays charged"
    );
    assert_eq!(
        budget.used_bytes(),
        0,
        "dropping the read releases the result"
    );
}

#[tokio::test]
async fn full_shared_budget_waits_then_succeeds() {
    let objects = objects(1);
    let format = ObjectFormat::Sha1;
    let written = pack(&objects, format, true);
    let pool = RemoteBufferPool::new(1024, 2).expect("pool");
    let budget = Arc::new(PackDecodeBudget::new(2 * 1024 * 1024));
    let db = RemoteObjectDatabase::open(
        probe(&written),
        pool.clone(),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open");
    let held = budget
        .reserve(2 * 1024 * 1024)
        .expect("first reader fills budget");
    let oid = objects[0].object_id(format).expect("oid");
    let mut read = std::pin::pin!(db.read_object(&oid));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), read.as_mut())
            .await
            .is_err(),
        "second reader must wait for budget instead of returning a limit error"
    );
    drop(held);
    let object = tokio::time::timeout(Duration::from_secs(2), read)
        .await
        .expect("reader resumes")
        .expect("decode");
    assert_eq!(object.as_ref(), &objects[0]);
    assert_eq!(budget.used_bytes(), 0);
    assert_eq!(pool.available_buffers(), 2);
}

#[tokio::test]
async fn cancelling_budget_waiter_leaves_capacity_for_next_reader() {
    let objects = objects(1);
    let format = ObjectFormat::Sha1;
    let written = pack(&objects, format, true);
    let pool = RemoteBufferPool::with_cpu_workers(1024, 4, 1).expect("pool");
    let budget = Arc::new(PackDecodeBudget::new(2 * 1024 * 1024));
    let db = RemoteObjectDatabase::open(
        probe(&written),
        pool.clone(),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open");
    let held = budget.reserve(budget.limit_bytes()).expect("fill budget");
    let oid = objects[0].object_id(format).expect("oid");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), db.read_object(&oid))
            .await
            .is_err()
    );
    assert_eq!(
        pool.available_buffers(),
        4,
        "waiting does not hold I/O slots"
    );
    assert_eq!(budget.used_bytes(), budget.limit_bytes());
    drop(held);
    let actual = tokio::time::timeout(Duration::from_secs(2), db.read_object(&oid))
        .await
        .expect("cancelled waiter removed")
        .expect("decode");
    assert_eq!(actual.as_ref(), &objects[0]);
    assert_eq!(budget.used_bytes(), 0);
}

#[tokio::test]
async fn decoder_limit_stays_typed_after_cpu_handoff() {
    let objects = objects(1);
    let format = ObjectFormat::Sha1;
    let written = pack(&objects, format, true);
    let mut small = limits();
    small.decode.max_materialized_bytes = 1;
    let budget = Arc::new(PackDecodeBudget::new(4 * 1024 * 1024));
    let db = RemoteObjectDatabase::open(
        probe(&written),
        RemoteBufferPool::new(1024, 2).expect("pool"),
        budget.clone(),
        location(&written),
        small,
    )
    .await
    .expect("open");
    assert!(
        matches!(db.read_object(&objects[0].object_id(format).expect("oid")).await,
        Err(RemoteOdbError::Decode(sley_pack::PackReadError::Limit(error)))
            if error.kind == sley_pack::PackLimitKind::MaterializedBytes && error.limit == 1)
    );
    assert_eq!(budget.used_bytes(), 0);
}

#[tokio::test]
async fn concurrent_closures_wait_without_holding_partial_reservations() {
    let objects = objects(4);
    let format = ObjectFormat::Sha1;
    let written = pack(&objects, format, false);
    // One complete closure fits, but two decoder reservations do not. The
    // configured encoded ceiling is larger than this actual pack.
    let cap = limits().decode.max_materialized_bytes + written.pack.len();
    let budget = Arc::new(PackDecodeBudget::new(cap));
    let db = RemoteObjectDatabase::open(
        probe(&written),
        RemoteBufferPool::new(1024, 4).expect("pool"),
        budget.clone(),
        location(&written),
        limits(),
    )
    .await
    .expect("open");
    let ids: Vec<_> = [3, 2, 1, 0]
        .into_iter()
        .map(|i| objects[i].object_id(format).expect("oid"))
        .collect();
    let actual = tokio::time::timeout(Duration::from_secs(5), db.read_objects(&ids))
        .await
        .expect("concurrent readers make progress")
        .expect("decode");
    for (object, i) in actual.iter().zip([3, 2, 1, 0]) {
        assert_eq!(object.as_ref(), &objects[i]);
    }
    assert!(budget.peak_bytes() <= cap);
    assert_eq!(budget.used_bytes(), 0);
}
