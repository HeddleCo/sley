//! sley-vs-git comparison for ref enumeration (`for-each-ref`, `show-ref`)
//! over a repository with many branch refs.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sley_bench::{CommitBenchFixture, create_commit_fixture, run_git, run_sley};
use std::sync::OnceLock;

fn fixture() -> &'static CommitBenchFixture {
    static FIXTURE: OnceLock<CommitBenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_commit_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("benchmark fixture setup failed: {err}"),
    })
}

fn for_each_ref(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("for_each_ref");
    group.bench_function("sley", |b| {
        b.iter(|| {
            let output = run_sley(&fixture.repo_root, &["for-each-ref"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley for-each-ref failed: {err}"),
            }
        });
    });
    group.bench_function("git", |b| {
        b.iter(|| {
            let output = run_git(&fixture.repo_root, &["for-each-ref"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("git for-each-ref failed: {err}"),
            }
        });
    });
    group.finish();
}

fn for_each_ref_format(c: &mut Criterion) {
    let fixture = fixture();
    let format = "%(refname:short) %(objectname:short) %(committerdate:iso)";
    let mut group = c.benchmark_group("for_each_ref_format");
    group.bench_function("sley", |b| {
        b.iter(|| {
            let output = run_sley(
                &fixture.repo_root,
                &["for-each-ref", &format!("--format={format}")],
                &[],
            );
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley for-each-ref --format failed: {err}"),
            }
        });
    });
    group.bench_function("git", |b| {
        b.iter(|| {
            let output = run_git(
                &fixture.repo_root,
                &["for-each-ref", &format!("--format={format}")],
                &[],
            );
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("git for-each-ref --format failed: {err}"),
            }
        });
    });
    group.finish();
}

fn show_ref(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("show_ref");
    group.bench_function("sley", |b| {
        b.iter(|| {
            let output = run_sley(&fixture.repo_root, &["show-ref"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley show-ref failed: {err}"),
            }
        });
    });
    group.bench_function("git", |b| {
        b.iter(|| {
            let output = run_git(&fixture.repo_root, &["show-ref"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("git show-ref failed: {err}"),
            }
        });
    });
    group.finish();
}

criterion_group!(benches, for_each_ref, for_each_ref_format, show_ref);
criterion_main!(benches);
