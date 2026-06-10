//! sley-vs-git comparison for `rev-parse` (many oids per invocation), plus the
//! sley-internal prefix-resolution path.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use sley_bench::{BenchFixture, FIXTURE_OBJECT_COUNT, create_fixture, run_git, run_sley};
use sley_odb::ObjectPrefixResolution;
use std::sync::OnceLock;

fn fixture() -> &'static BenchFixture {
    static FIXTURE: OnceLock<BenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("benchmark fixture setup failed: {err}"),
    })
}

fn rev_parse_args(input: &[u8]) -> Vec<String> {
    let mut args = vec!["rev-parse".to_string()];
    for line in std::str::from_utf8(input)
        .expect("batch input is ascii hex")
        .lines()
    {
        args.push(line.to_string());
    }
    args
}

fn rev_parse_oid_resolve(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("rev_parse_oid_resolve");

    for count in [1usize, 100, FIXTURE_OBJECT_COUNT] {
        let input = fixture.batch_input(count);
        group.throughput(Throughput::Elements(count as u64));

        group.bench_with_input(BenchmarkId::new("sley", count), &input, |b, input| {
            b.iter(|| {
                let args = rev_parse_args(input);
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let output = run_sley(&fixture.repo_root, &arg_refs, &[]);
                match output {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("sley rev-parse failed: {err}"),
                }
            });
        });

        group.bench_with_input(BenchmarkId::new("git", count), &input, |b, input| {
            b.iter(|| {
                let args = rev_parse_args(input);
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let output = run_git(&fixture.repo_root, &arg_refs, &[]);
                match output {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("git rev-parse failed: {err}"),
                }
            });
        });

        group.bench_with_input(
            BenchmarkId::new("odb_resolve_prefix", count),
            &input,
            |b, input| {
                let db = fixture.database();
                b.iter(|| {
                    let mut resolved = 0usize;
                    for line in std::str::from_utf8(input)
                        .expect("batch input is ascii hex")
                        .lines()
                    {
                        match db.resolve_prefix(black_box(line)) {
                            Ok(ObjectPrefixResolution::Unique(_)) => resolved += 1,
                            Ok(ObjectPrefixResolution::Ambiguous(_)) => {
                                panic!("unexpected ambiguous oid prefix for {line}")
                            }
                            Ok(ObjectPrefixResolution::Missing) => {
                                panic!("missing oid prefix for {line}")
                            }
                            Err(err) => panic!("resolve_prefix failed for {line}: {err}"),
                        }
                    }
                    black_box(resolved)
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, rev_parse_oid_resolve);
criterion_main!(benches);
