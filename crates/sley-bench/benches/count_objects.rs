//! sley-vs-git comparison for `count-objects -v` on a packed repository.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sley_bench::{BenchFixture, create_fixture, run_git, run_sley};
use std::sync::OnceLock;

fn fixture() -> &'static BenchFixture {
    static FIXTURE: OnceLock<BenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("benchmark fixture setup failed: {err}"),
    })
}

fn count_objects_verbose_packed(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("count_objects_verbose_packed");

    group.bench_function("sley", |b| {
        b.iter(|| {
            let output = run_sley(&fixture.repo_root, &["count-objects", "-v"], &[]);
            match output {
                Ok(body) => black_box(body.len()),
                Err(err) => panic!("sley count-objects -v failed: {err}"),
            }
        });
    });

    group.bench_function("git", |b| {
        b.iter(|| {
            let output = run_git(&fixture.repo_root, &["count-objects", "-v"], &[]);
            match output {
                Ok(body) => black_box(body.len()),
                Err(err) => panic!("git count-objects -v failed: {err}"),
            }
        });
    });

    group.finish();
}

criterion_group!(benches, count_objects_verbose_packed);
criterion_main!(benches);
