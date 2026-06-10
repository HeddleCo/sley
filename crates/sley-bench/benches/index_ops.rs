//! sley-vs-git comparison for index-adjacent plumbing on a 1k-file worktree:
//! `ls-files`, `ls-files -s`, `update-index --refresh`, and
//! `hash-object --stdin-paths` (200 paths in one process).

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sley_bench::{WorktreeBenchFixture, create_worktree_fixture, run_git, run_sley};
use std::sync::OnceLock;

/// Shared read-only fixture for `ls-files` / `hash-object`.
fn shared_fixture() -> &'static WorktreeBenchFixture {
    static FIXTURE: OnceLock<WorktreeBenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_worktree_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("worktree fixture setup failed: {err}"),
    })
}

fn ls_files(c: &mut Criterion) {
    let fixture = shared_fixture();
    let mut group = c.benchmark_group("ls_files");
    group.bench_function("sley", |b| {
        b.iter(|| match run_sley(&fixture.repo_root, &["ls-files"], &[]) {
            Ok(body) => black_box(body.len()),
            Err(err) => panic!("sley ls-files failed: {err}"),
        });
    });
    group.bench_function("git", |b| {
        b.iter(|| match run_git(&fixture.repo_root, &["ls-files"], &[]) {
            Ok(body) => black_box(body.len()),
            Err(err) => panic!("git ls-files failed: {err}"),
        });
    });
    group.finish();
}

fn ls_files_stage(c: &mut Criterion) {
    let fixture = shared_fixture();
    let mut group = c.benchmark_group("ls_files_stage");
    group.bench_function("sley", |b| {
        b.iter(
            || match run_sley(&fixture.repo_root, &["ls-files", "-s"], &[]) {
                Ok(body) => black_box(body.len()),
                Err(err) => panic!("sley ls-files -s failed: {err}"),
            },
        );
    });
    group.bench_function("git", |b| {
        b.iter(
            || match run_git(&fixture.repo_root, &["ls-files", "-s"], &[]) {
                Ok(body) => black_box(body.len()),
                Err(err) => panic!("git ls-files -s failed: {err}"),
            },
        );
    });
    group.finish();
}

fn update_index_refresh(c: &mut Criterion) {
    // `update-index --refresh` may rewrite the index; give each arm its own
    // fixture so the binaries never share mutable state.
    let mut group = c.benchmark_group("update_index_refresh");
    {
        let fixture = create_worktree_fixture().expect("sley-arm worktree fixture");
        group.bench_function("sley", |b| {
            b.iter(
                || match run_sley(&fixture.repo_root, &["update-index", "--refresh"], &[]) {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("sley update-index --refresh failed: {err}"),
                },
            );
        });
    }
    {
        let fixture = create_worktree_fixture().expect("git-arm worktree fixture");
        group.bench_function("git", |b| {
            b.iter(
                || match run_git(&fixture.repo_root, &["update-index", "--refresh"], &[]) {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("git update-index --refresh failed: {err}"),
                },
            );
        });
    }
    group.finish();
}

fn hash_object_stdin_paths(c: &mut Criterion) {
    let fixture = shared_fixture();
    let count = 200.min(fixture.tracked_files.len());
    let mut stdin = String::new();
    for path in &fixture.tracked_files[..count] {
        stdin.push_str(path);
        stdin.push('\n');
    }
    let stdin = stdin.into_bytes();

    let mut group = c.benchmark_group("hash_object_stdin_paths");
    group.throughput(criterion::Throughput::Elements(count as u64));
    group.bench_function("sley", |b| {
        b.iter(|| {
            match run_sley(
                &fixture.repo_root,
                &["hash-object", "--stdin-paths"],
                &stdin,
            ) {
                Ok(body) => black_box(body.len()),
                Err(err) => panic!("sley hash-object --stdin-paths failed: {err}"),
            }
        });
    });
    group.bench_function("git", |b| {
        b.iter(|| {
            match run_git(
                &fixture.repo_root,
                &["hash-object", "--stdin-paths"],
                &stdin,
            ) {
                Ok(body) => black_box(body.len()),
                Err(err) => panic!("git hash-object --stdin-paths failed: {err}"),
            }
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    ls_files,
    ls_files_stage,
    update_index_refresh,
    hash_object_stdin_paths
);
criterion_main!(benches);
