//! Repack material-efficiency benchmarks.
//!
//! Deterministic verification and quality gates run once before Criterion.
//! Timed bodies contain only the implementation named by each benchmark.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use sley_bench::repack_measure::{
    PackWriterRegressionContract, REPACK_QUALITY_FLOOR_PERCENT, REPOSITORY_BASELINE_DELTA_COUNT,
    REPOSITORY_BASELINE_INDEX_BYTES, REPOSITORY_BASELINE_PACK_BYTES,
    REPOSITORY_BASELINE_PREPARATION_READS, REPOSITORY_REPACK_OBJECT_COUNT,
    RepositoryRepackRegressionContract, create_repository_repack_fixture, legacy_fixture_repack,
    measure_pack_writer, measure_repository_repack, prepare_fixture_repack,
};
use sley_core::{ByteBudget, GitError, ObjectFormat};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{PackFile, PackWriteLimits, PackWriteOptions};
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Arc;

const WRITER_OBJECT_COUNT: usize = 256;
const WRITER_BASELINE_PEAK_BYTES: u64 = 510_291;
const WRITER_BASELINE_PACK_BYTES: u64 = 5_898;
const WRITER_BASELINE_DELTA_COUNT: u32 = 254;

const fn quality_floor_min(baseline: u64) -> u64 {
    baseline
        .saturating_mul(REPACK_QUALITY_FLOOR_PERCENT)
        .div_ceil(100)
}

const fn quality_floor_max(baseline: u64) -> u64 {
    baseline
        .saturating_mul(100)
        .div_ceil(REPACK_QUALITY_FLOOR_PERCENT)
}

fn bench_repack_measure(c: &mut Criterion) {
    bench_pack_writer(c);
    bench_repository_repack(c);
}

fn bench_pack_writer(c: &mut Criterion) {
    let format = ObjectFormat::Sha1;
    let objects = (0..WRITER_OBJECT_COUNT)
        .map(|index| {
            let mut body = vec![b'r'; 16 * 1024];
            body.extend_from_slice(format!("variant-{index:04}\n").as_bytes());
            EncodedObject::new(ObjectType::Blob, body)
        })
        .collect::<Vec<_>>();
    let decoded_bytes = objects.iter().map(|object| object.body.len() as u64).sum();
    let mut ids = Vec::with_capacity(objects.len());
    let mut by_id = HashMap::with_capacity(objects.len());
    for object in objects {
        let oid = object.object_id(format).expect("fixture object id");
        ids.push(oid);
        by_id.insert(oid, Arc::new(object));
    }
    let options = PackWriteOptions::new()
        .with_window(16)
        .with_depth(16)
        .with_reorder(false);
    let limits = PackWriteLimits::new()
        .with_compression_working_set(ByteBudget::new(256 * 1024))
        .with_delta_base(ByteBudget::new(256 * 1024))
        .with_decoded_object(ByteBudget::new(32 * 1024));
    let measure = || {
        measure_pack_writer(
            ids.iter().copied(),
            ids.len() as u32,
            format,
            &options,
            limits,
            |oid| {
                by_id
                    .get(oid)
                    .cloned()
                    .ok_or_else(|| GitError::not_found(format!("fixture object {oid}")))
            },
        )
        .expect("measure pack writer")
    };
    let measured = measure();
    let contract = PackWriterRegressionContract {
        expected_object_count: WRITER_OBJECT_COUNT as u32,
        max_object_reads: WRITER_OBJECT_COUNT as u64,
        max_decoded_bytes: decoded_bytes,
        max_peak_charged_writer_bytes: quality_floor_max(WRITER_BASELINE_PEAK_BYTES),
        max_pack_size: quality_floor_max(WRITER_BASELINE_PACK_BYTES),
        min_delta_count: quality_floor_min(u64::from(WRITER_BASELINE_DELTA_COUNT)) as u32,
        max_delta_depth: options.depth as u32,
    };
    contract.check(measured).expect("pack-writer contract");
    eprintln!(
        "pack_writer: objects={} reads={} decoded_bytes={} charged_writer_peak={} \
         pack_size={} deltas={} max_delta_depth={}",
        measured.object_count,
        measured.object_reads,
        measured.decoded_bytes,
        measured.peak_charged_writer_bytes,
        measured.pack_size,
        measured.delta_count,
        measured.max_delta_depth,
    );

    let mut group = c.benchmark_group("repack_pack_writer");
    group.throughput(Throughput::Bytes(decoded_bytes));
    group.bench_function("source_to_vec", |b| {
        b.iter(|| {
            let mut pack = Vec::new();
            let summary = PackFile::write_packed_from_source_to_writer(
                ids.iter().copied(),
                ids.len() as u32,
                format,
                &options,
                limits,
                |oid| {
                    by_id
                        .get(oid)
                        .cloned()
                        .ok_or_else(|| GitError::not_found(format!("fixture object {oid}")))
                },
                &mut pack,
            )
            .expect("write pack");
            black_box((summary, pack))
        })
    });
    group.finish();
}

fn bench_repository_repack(c: &mut Criterion) {
    let fixture = create_repository_repack_fixture().expect("repository repack fixture");
    let measured = measure_repository_repack(&fixture).expect("measure repository repack");
    let contract = RepositoryRepackRegressionContract {
        expected_object_count: REPOSITORY_REPACK_OBJECT_COUNT as u32,
        max_preparation_body_reads: REPOSITORY_BASELINE_PREPARATION_READS,
        max_staged_pack_size: quality_floor_max(REPOSITORY_BASELINE_PACK_BYTES),
        expected_index_bytes: REPOSITORY_BASELINE_INDEX_BYTES,
        min_prepared_delta_count: quality_floor_min(u64::from(REPOSITORY_BASELINE_DELTA_COUNT))
            as u32,
        minimum_quality_percent: REPACK_QUALITY_FLOOR_PERCENT,
        max_delta_depth: 50,
    };
    eprintln!(
        "repository_repack: objects={} preparation_reads={} prepared_resident_pack={} \
         staged_pack={} prepared_index={} prepared_deltas={} prepared_depth={} \
         legacy_resident_pack={} legacy_pack={} legacy_index={} legacy_deltas={} \
         legacy_depth={} checksum_identical={} index_identical={}",
        measured.object_count,
        measured.preparation_body_reads,
        measured.prepared_resident_pack_output_bytes,
        measured.staged_pack_size,
        measured.prepared_index_bytes,
        measured.prepared_delta_count,
        measured.prepared_max_delta_depth,
        measured.legacy_resident_pack_output_bytes,
        measured.legacy_pack_size,
        measured.legacy_index_bytes,
        measured.legacy_delta_count,
        measured.legacy_max_delta_depth,
        measured.checksum_identical,
        measured.index_identical,
    );
    contract
        .check(measured)
        .expect("repository repack contract");

    let mut group = c.benchmark_group("repository_repack");
    group.throughput(Throughput::Elements(REPOSITORY_REPACK_OBJECT_COUNT as u64));
    group.sample_size(10);
    group.bench_function("legacy_in_memory", |b| {
        b.iter(|| black_box(legacy_fixture_repack(&fixture).expect("legacy repack")))
    });
    group.bench_function("prepared_file_backed", |b| {
        b.iter(|| black_box(prepare_fixture_repack(&fixture).expect("prepared repack")))
    });
    group.finish();
}

criterion_group!(benches, bench_repack_measure);
criterion_main!(benches);
