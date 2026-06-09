use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use sley_bench::{BenchFixture, FIXTURE_OBJECT_COUNT, create_fixture};
use sley_core::{GitError, Result};
use sley_odb::ObjectReader;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

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

fn cat_file_p_single(c: &mut Criterion) {
    let fixture = fixture();
    let oid = fixture.sample_oid.to_hex();

    let mut group = c.benchmark_group("cat_file_p_single_packed");
    group.bench_function("sley_cli", |b| {
        b.iter(|| {
            let output = run_sley(&fixture.repo_root, &["cat-file", "-p", oid.as_str()], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley cat-file -p failed: {err}"),
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
        group.bench_with_input(BenchmarkId::new("sley_cli", count), &input, |b, input| {
            b.iter(|| {
                let output = run_sley(&fixture.repo_root, &["cat-file", "--batch-check"], input);
                match output {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("sley cat-file --batch-check failed: {err}"),
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
        group.bench_with_input(BenchmarkId::new("sley_cli", count), &input, |b, input| {
            b.iter(|| {
                let output = run_sley(&fixture.repo_root, &["cat-file", "--batch"], input);
                match output {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("sley cat-file --batch failed: {err}"),
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
