//! sley-vs-git comparison for recursive tree listing (`ls-tree -r HEAD`).

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

fn ls_tree_recursive_head(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("ls_tree_recursive_head");
    group.bench_function("sley", |b| {
        b.iter(|| {
            let output = run_sley(&fixture.repo_root, &["ls-tree", "-r", "HEAD"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley ls-tree -r HEAD failed: {err}"),
            }
        });
    });
    group.bench_function("git", |b| {
        b.iter(|| {
            let output = run_git(&fixture.repo_root, &["ls-tree", "-r", "HEAD"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("git ls-tree -r HEAD failed: {err}"),
            }
        });
    });
    group.finish();
}

criterion_group!(benches, ls_tree_recursive_head);
criterion_main!(benches);
