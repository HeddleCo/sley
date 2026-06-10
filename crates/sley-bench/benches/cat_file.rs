//! sley-vs-git comparison for `cat-file`, plus sley-internal ODB read paths.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use sley_bench::{BenchFixture, FIXTURE_OBJECT_COUNT, create_fixture, run_git, run_sley};
use sley_odb::ObjectReader;
use std::sync::OnceLock;

fn fixture() -> &'static BenchFixture {
    static FIXTURE: OnceLock<BenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("benchmark fixture setup failed: {err}"),
    })
}

fn cat_file_p_single(c: &mut Criterion) {
    let fixture = fixture();
    let oid = fixture.sample_oid.to_hex();

    let mut group = c.benchmark_group("cat_file_p_single_packed");
    group.bench_function("sley", |b| {
        b.iter(|| {
            let output = run_sley(&fixture.repo_root, &["cat-file", "-p", oid.as_str()], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley cat-file -p failed: {err}"),
            }
        });
    });

    group.bench_function("git", |b| {
        b.iter(|| {
            let output = run_git(&fixture.repo_root, &["cat-file", "-p", oid.as_str()], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("git cat-file -p failed: {err}"),
            }
        });
    });

    group.bench_function("odb_read_object", |b| {
        let db = fixture.database();
        let oid = fixture.sample_oid.clone();
        b.iter(|| match db.read_object(black_box(&oid)) {
            Ok(object) => black_box(object.body.len()),
            Err(err) => panic!("read_object failed: {err}"),
        });
    });
    group.finish();
}

fn cat_file_batch_check(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("cat_file_batch_check");

    for count in [100usize, FIXTURE_OBJECT_COUNT] {
        let input = fixture.batch_input(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("sley", count), &input, |b, input| {
            b.iter(|| {
                let output = run_sley(&fixture.repo_root, &["cat-file", "--batch-check"], input);
                match output {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("sley cat-file --batch-check failed: {err}"),
                }
            });
        });
        group.bench_with_input(BenchmarkId::new("git", count), &input, |b, input| {
            b.iter(|| {
                let output = run_git(&fixture.repo_root, &["cat-file", "--batch-check"], input);
                match output {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("git cat-file --batch-check failed: {err}"),
                }
            });
        });
    }

    group.finish();
}

fn cat_file_batch_with_content(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("cat_file_batch");

    for count in [100usize, FIXTURE_OBJECT_COUNT] {
        let input = fixture.batch_input(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("sley", count), &input, |b, input| {
            b.iter(|| {
                let output = run_sley(&fixture.repo_root, &["cat-file", "--batch"], input);
                match output {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("sley cat-file --batch failed: {err}"),
                }
            });
        });
        group.bench_with_input(BenchmarkId::new("git", count), &input, |b, input| {
            b.iter(|| {
                let output = run_git(&fixture.repo_root, &["cat-file", "--batch"], input);
                match output {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("git cat-file --batch failed: {err}"),
                }
            });
        });
    }

    group.finish();
}

fn odb_read_header_vs_read_object(c: &mut Criterion) {
    let fixture = fixture();
    let db = fixture.database();
    let mut group = c.benchmark_group("odb_read_header_vs_read_object");

    for count in [100usize, FIXTURE_OBJECT_COUNT] {
        let oids = fixture.object_ids[..count].to_vec();
        group.throughput(Throughput::Elements(count as u64));

        group.bench_with_input(
            BenchmarkId::new("read_object_header", count),
            &oids,
            |b, oids| {
                b.iter(|| {
                    let mut total = 0u64;
                    for oid in oids {
                        match db.read_object_header(black_box(oid)) {
                            Ok(Some((_, size))) => total = total.wrapping_add(size),
                            Ok(None) => panic!("missing object {oid}"),
                            Err(err) => panic!("read_object_header failed: {err}"),
                        }
                    }
                    black_box(total)
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("read_object", count), &oids, |b, oids| {
            b.iter(|| {
                let mut total = 0u64;
                for oid in oids {
                    match db.read_object(black_box(oid)) {
                        Ok(object) => total = total.wrapping_add(object.body.len() as u64),
                        Err(err) => panic!("read_object failed: {err}"),
                    }
                }
                black_box(total)
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    cat_file_p_single,
    cat_file_batch_check,
    cat_file_batch_with_content,
    odb_read_header_vs_read_object
);
criterion_main!(benches);
