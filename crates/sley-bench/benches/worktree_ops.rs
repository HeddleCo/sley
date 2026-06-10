//! sley-vs-git comparison for porcelain over a 1k-file worktree:
//! `status --porcelain`, `add -u` (after touching 10 files), and
//! `commit --allow-empty`.
//!
//! Every group builds ONE FIXTURE PER ARM: these commands mutate repository
//! state (index refresh, staged entries, new commits), so the binaries must
//! never share a repo.

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use sley_bench::{WorktreeBenchFixture, create_worktree_fixture, run_git, run_sley};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

type Runner = fn(&Path, &[&str], &[u8]) -> sley_core::Result<Vec<u8>>;

fn status_porcelain(c: &mut Criterion) {
    let mut group = c.benchmark_group("status_porcelain");
    for (label, runner) in [("sley", run_sley as Runner), ("git", run_git as Runner)] {
        let fixture = create_worktree_fixture().expect("worktree fixture");
        group.bench_function(label, |b| {
            b.iter(
                || match runner(&fixture.repo_root, &["status", "--porcelain"], &[]) {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("{label} status --porcelain failed: {err}"),
                },
            );
        });
    }
    group.finish();
}

/// Touch 10 tracked files with fresh content so `add -u` has real work.
fn dirty_files(fixture: &WorktreeBenchFixture, revision: u64) {
    for path in fixture.tracked_files.iter().take(10) {
        fs::write(
            fixture.repo_root.join(path),
            format!("bench revision {revision} of {path}\n"),
        )
        .expect("dirty tracked file");
    }
}

fn add_update(c: &mut Criterion) {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    let mut group = c.benchmark_group("add_update_10_dirty");
    for (label, runner) in [("sley", run_sley as Runner), ("git", run_git as Runner)] {
        let fixture = create_worktree_fixture().expect("worktree fixture");
        group.bench_function(label, |b| {
            b.iter_batched(
                || dirty_files(&fixture, REVISION.fetch_add(1, Ordering::Relaxed)),
                |()| match runner(&fixture.repo_root, &["add", "-u"], &[]) {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("{label} add -u failed: {err}"),
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn commit_allow_empty(c: &mut Criterion) {
    let mut group = c.benchmark_group("commit_allow_empty");
    for (label, runner) in [("sley", run_sley as Runner), ("git", run_git as Runner)] {
        let fixture = create_worktree_fixture().expect("worktree fixture");
        group.bench_function(label, |b| {
            b.iter(|| {
                match runner(
                    &fixture.repo_root,
                    &["commit", "-q", "--allow-empty", "-m", "bench"],
                    &[],
                ) {
                    Ok(body) => black_box(body.len()),
                    Err(err) => panic!("{label} commit --allow-empty failed: {err}"),
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, status_porcelain, add_update, commit_allow_empty);
criterion_main!(benches);
