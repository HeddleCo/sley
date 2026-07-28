use criterion::{BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use sley_bench::{
    BenchFixture, FIXTURE_OBJECT_COUNT, LARGE_FIXTURE_OBJECT_COUNT, MEDIUM_FIXTURE_OBJECT_COUNT,
    create_fixture, create_fixture_with_count,
};
use sley_core::{GitError, Result};
use sley_odb::ObjectPrefixResolution;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

fn run_sley(cwd: &Path, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new(env!("SLEY_BENCH_BIN"))
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !stdin.is_empty() {
        let stdin_handle = child
            .stdin
            .as_mut()
            .ok_or_else(|| GitError::Command("missing sley stdin".into()))?;
        stdin_handle
            .write_all(stdin)
            .map_err(|err| GitError::Io(err.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "sley {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn fixture() -> &'static BenchFixture {
    static FIXTURE: OnceLock<BenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("benchmark fixture setup failed: {err}"),
    })
}

fn medium_fixture() -> &'static BenchFixture {
    static FIXTURE: OnceLock<BenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(
        || match create_fixture_with_count(MEDIUM_FIXTURE_OBJECT_COUNT) {
            Ok(fixture) => fixture,
            Err(err) => panic!("medium benchmark fixture setup failed: {err}"),
        },
    )
}

fn large_fixture() -> &'static BenchFixture {
    static FIXTURE: OnceLock<BenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(
        || match create_fixture_with_count(LARGE_FIXTURE_OBJECT_COUNT) {
            Ok(fixture) => fixture,
            Err(err) => panic!("large benchmark fixture setup failed: {err}"),
        },
    )
}

fn rev_parse_oid_resolve(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("rev_parse_oid_resolve");

    for count in [1usize, 100, FIXTURE_OBJECT_COUNT] {
        let input = fixture.batch_input(count);
        group.throughput(Throughput::Elements(count as u64));

        group.bench_with_input(BenchmarkId::new("sley_cli", count), &input, |b, input| {
            b.iter(|| {
                let mut args = vec!["rev-parse".to_string()];
                for line in std::str::from_utf8(input)
                    .expect("benchmark input should be UTF-8")
                    .lines()
                {
                    args.push(line.to_string());
                }
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let output = run_sley(&fixture.repo_root, &arg_refs, &[]);
                match output {
                    Ok(body) => std::hint::black_box(body.len()),
                    Err(err) => panic!("sley rev-parse failed: {err}"),
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
                        .expect("benchmark input should be UTF-8")
                        .lines()
                    {
                        match db.resolve_prefix(std::hint::black_box(line)) {
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
                    std::hint::black_box(resolved)
                });
            },
        );
    }

    group.finish();
}

fn rev_parse_oid_resolve_1k(c: &mut Criterion) {
    let fixture = medium_fixture();
    let count = MEDIUM_FIXTURE_OBJECT_COUNT.min(fixture.object_ids.len());
    if count < MEDIUM_FIXTURE_OBJECT_COUNT {
        return;
    }
    let input = fixture.batch_input(count);
    let mut group = c.benchmark_group("rev_parse_oid_resolve_1k");
    group.throughput(Throughput::Elements(count as u64));
    group.bench_with_input(
        BenchmarkId::new("odb_resolve_prefix", count),
        &input,
        |b, input| {
            let db = fixture.database();
            b.iter(|| {
                let mut resolved = 0usize;
                for line in std::str::from_utf8(input)
                    .expect("benchmark input should be UTF-8")
                    .lines()
                {
                    match db.resolve_prefix(std::hint::black_box(line)) {
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
                std::hint::black_box(resolved)
            });
        },
    );
    group.finish();
}

fn rev_parse_oid_resolve_100k(c: &mut Criterion) {
    let fixture = large_fixture();
    let count = LARGE_FIXTURE_OBJECT_COUNT.min(fixture.object_ids.len());
    if count < LARGE_FIXTURE_OBJECT_COUNT {
        return;
    }
    let input = fixture.batch_input(count);
    let mut group = c.benchmark_group("rev_parse_oid_resolve_100k");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(count as u64));
    group.bench_with_input(
        BenchmarkId::new("odb_resolve_prefix", count),
        &input,
        |b, input| {
            let db = fixture.database();
            b.iter(|| {
                let mut resolved = 0usize;
                for line in std::str::from_utf8(input)
                    .expect("benchmark input should be UTF-8")
                    .lines()
                {
                    match db.resolve_prefix(std::hint::black_box(line)) {
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
                std::hint::black_box(resolved)
            });
        },
    );
    group.finish();
}

criterion_group!(
    benches,
    rev_parse_oid_resolve,
    rev_parse_oid_resolve_1k,
    rev_parse_oid_resolve_100k
);
criterion_main!(benches);
