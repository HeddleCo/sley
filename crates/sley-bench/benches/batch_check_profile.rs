//! Component breakdown for cat-file --batch-check hot path.
//!
//! ```text
//! cargo bench -p sley-bench --bench batch_check_profile -- --quick
//! ```

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use sley_bench::{BenchFixture, FIXTURE_OBJECT_COUNT, create_fixture};
use sley_core::ObjectId;
use sley_object::ObjectType;
use sley_odb::ObjectReader;
use std::io::Write;
use std::sync::OnceLock;

fn fixture() -> &'static BenchFixture {
    static FIXTURE: OnceLock<BenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("benchmark fixture setup failed: {err}"),
    })
}

fn format_batch_line(oid: &ObjectId, object_type: ObjectType, size: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    writeln!(out, "{oid} {} {size}", object_type.as_str()).expect("format batch line");
    out
}

fn profile_find_and_header(c: &mut Criterion) {
    let fixture = fixture();
    let db = fixture.database();
    let oids = &fixture.object_ids[..FIXTURE_OBJECT_COUNT];
    let format = fixture.format;

    let mut group = c.benchmark_group("batch_check_components");
    group.throughput(Throughput::Elements(FIXTURE_OBJECT_COUNT as u64));

    group.bench_function("from_hex_only", |b| {
        let lines = fixture.batch_input(FIXTURE_OBJECT_COUNT);
        b.iter(|| {
            let mut count = 0usize;
            for line in std::str::from_utf8(&lines)
                .expect("benchmark input should be UTF-8")
                .lines()
            {
                if ObjectId::from_hex(format, black_box(line)).is_ok() {
                    count += 1;
                }
            }
            black_box(count)
        });
    });

    group.bench_function("read_object_header", |b| {
        b.iter(|| {
            let mut total = 0u64;
            for oid in oids {
                total = total.wrapping_add(
                    db.read_object_header(black_box(oid))
                        .expect("read_object_header")
                        .expect("missing object")
                        .1,
                );
            }
            black_box(total)
        });
    });

    group.bench_function("read_object_full", |b| {
        b.iter(|| {
            let mut total = 0u64;
            for oid in oids {
                total = total.wrapping_add(
                    db.read_object(black_box(oid))
                        .expect("read_object")
                        .body
                        .len() as u64,
                );
            }
            black_box(total)
        });
    });

    group.bench_function("header_plus_format_line", |b| {
        b.iter(|| {
            let mut out_len = 0usize;
            for oid in oids {
                let (object_type, size) = db
                    .read_object_header(black_box(oid))
                    .expect("read_object_header")
                    .expect("missing object");
                let line = format_batch_line(oid, object_type, size);
                out_len = out_len.wrapping_add(line.len());
            }
            black_box(out_len)
        });
    });

    group.finish();
}

criterion_group!(benches, profile_find_and_header);
criterion_main!(benches);
