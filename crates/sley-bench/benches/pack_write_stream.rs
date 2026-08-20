//! Streaming pack-write memory and throughput.
//!
//! Reports criterion throughput plus process RSS high-water (`VmHWM`) and the
//! writer's charged working-set peak.
//!
//! Representative slice (used by this file's default tiny count):
//!
//! ```text
//! cargo bench -p sley-bench --bench pack_write_stream -- --quick
//! ```
//!
//! Full one-million tiny-object run:
//!
//! ```text
//! SLEY_PACK_WRITE_STREAM_TINY=1000000 cargo bench -p sley-bench --bench pack_write_stream -- --quick
//! ```
//!
//! Heterogeneous and large-blob scenarios are always included.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use sley_core::{ByteBudget, ObjectFormat};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{PackFile, PackWriteLimits, PackWriteOptions};
use std::collections::HashMap;
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

fn tiny_count() -> usize {
    env::var("SLEY_PACK_WRITE_STREAM_TINY")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2_000)
}

fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("VmHWM:") else {
            continue;
        };
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        return Some(kb.saturating_mul(1024));
    }
    None
}

fn report_memory(label: &str, object_count: usize, elapsed: std::time::Duration, charged: u64) {
    let rss = peak_rss_bytes()
        .map(|bytes| format!("{} KiB", bytes / 1024))
        .unwrap_or_else(|| "unavailable".into());
    let per_sec = if elapsed.as_secs_f64() > 0.0 {
        object_count as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    eprintln!(
        "pack_write_stream {label}: objects={object_count} elapsed={elapsed:?} \
         throughput={per_sec:.0} objects/s charged_peak={charged} B rss_hwm={rss}"
    );
}

fn blob_map(
    format: ObjectFormat,
    bodies: impl IntoIterator<Item = Vec<u8>>,
) -> (
    Vec<sley_core::ObjectId>,
    HashMap<sley_core::ObjectId, Arc<EncodedObject>>,
) {
    let mut ids = Vec::new();
    let mut map = HashMap::new();
    for body in bodies {
        let object = EncodedObject::new(ObjectType::Blob, body);
        let oid = object.object_id(format).expect("bench oid");
        ids.push(oid);
        map.insert(oid, Arc::new(object));
    }
    (ids, map)
}

fn write_stream(
    format: ObjectFormat,
    ids: &[sley_core::ObjectId],
    map: &HashMap<sley_core::ObjectId, Arc<EncodedObject>>,
    limits: PackWriteLimits,
) -> u64 {
    let object_count = u32::try_from(ids.len()).expect("object count fits pack header");
    let mut written = Vec::new();
    let summary = PackFile::write_packed_from_source_to_writer(
        ids.iter().copied(),
        object_count,
        format,
        &PackWriteOptions::new().with_reorder(false).with_depth(0),
        limits,
        |oid| {
            map.get(oid)
                .cloned()
                .ok_or_else(|| sley_core::GitError::not_found(format!("missing {oid}")))
        },
        &mut written,
    )
    .expect("stream write");
    black_box(written.len());
    summary.peak_working_set_bytes
}

fn bench_pack_write_stream(c: &mut Criterion) {
    let format = ObjectFormat::Sha1;
    let tiny = tiny_count();
    let (tiny_ids, tiny_map) = blob_map(
        format,
        (0..tiny).map(|idx| format!("t{idx}\n").into_bytes()),
    );
    let (hetero_ids, hetero_map) = blob_map(
        format,
        (0..64u32).map(|idx| {
            let size = match idx % 5 {
                0 => 16,
                1 => 256,
                2 => 4 * 1024,
                3 => 64 * 1024,
                _ => 8,
            };
            let mut body = vec![b'h'; size];
            body.extend_from_slice(&idx.to_le_bytes());
            body
        }),
    );
    let (large_ids, large_map) = blob_map(
        format,
        (0..4u32).map(|idx| {
            let mut body = vec![b'L'; 1024 * 1024];
            body.extend_from_slice(&idx.to_le_bytes());
            body
        }),
    );
    let limits = PackWriteLimits::new()
        .with_compression_working_set(ByteBudget::new(4 * 1024 * 1024))
        .with_delta_base(ByteBudget::new(2 * 1024 * 1024))
        .with_decoded_object(ByteBudget::new(8 * 1024 * 1024));

    let started = Instant::now();
    let charged = write_stream(format, &tiny_ids, &tiny_map, limits);
    report_memory("tiny", tiny, started.elapsed(), charged);
    let started = Instant::now();
    let charged = write_stream(format, &hetero_ids, &hetero_map, limits);
    report_memory(
        "heterogeneous",
        hetero_ids.len(),
        started.elapsed(),
        charged,
    );
    let started = Instant::now();
    let charged = write_stream(format, &large_ids, &large_map, limits);
    report_memory("large_blobs", large_ids.len(), started.elapsed(), charged);

    let mut group = c.benchmark_group("pack_write_stream");
    group.throughput(Throughput::Elements(tiny as u64));
    group.bench_function("tiny_objects", |b| {
        b.iter(|| write_stream(format, &tiny_ids, &tiny_map, limits))
    });
    group.throughput(Throughput::Elements(hetero_ids.len() as u64));
    group.bench_function("heterogeneous", |b| {
        b.iter(|| write_stream(format, &hetero_ids, &hetero_map, limits))
    });
    group.throughput(Throughput::Elements(large_ids.len() as u64));
    group.bench_function("large_blobs", |b| {
        b.iter(|| write_stream(format, &large_ids, &large_map, limits))
    });
    group.finish();
}

criterion_group!(benches, bench_pack_write_stream);
criterion_main!(benches);
