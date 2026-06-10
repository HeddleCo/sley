//! sley-vs-git comparison for `rev-list` over a linear history with a
//! commit-graph present.

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

fn rev_list_count_head(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("rev_list_count_head");
    group.bench_function("sley", |b| {
        b.iter(|| {
            let output = run_sley(&fixture.repo_root, &["rev-list", "--count", "HEAD"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley rev-list --count HEAD failed: {err}"),
            }
        });
    });
    group.bench_function("git", |b| {
        b.iter(|| {
            let output = run_git(&fixture.repo_root, &["rev-list", "--count", "HEAD"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("git rev-list --count HEAD failed: {err}"),
            }
        });
    });
    group.finish();
}

fn rev_list_oneline_head(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("rev_list_oneline_head");
    group.bench_function("sley", |b| {
        b.iter(|| {
            let output = run_sley(
                &fixture.repo_root,
                &["rev-list", "--oneline", "-100", "HEAD"],
                &[],
            );
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley rev-list --oneline -100 HEAD failed: {err}"),
            }
        });
    });
    group.bench_function("git", |b| {
        b.iter(|| {
            let output = run_git(
                &fixture.repo_root,
                &["rev-list", "--oneline", "-100", "HEAD"],
                &[],
            );
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("git rev-list --oneline -100 HEAD failed: {err}"),
            }
        });
    });
    group.finish();
}

criterion_group!(benches, rev_list_count_head, rev_list_oneline_head);
criterion_main!(benches);
