//! sley-vs-git comparison for `init` into a fresh directory.
//!
//! Each iteration creates a unique directory, inits, and removes it again so
//! the suite does not litter /tmp with thousands of repos. The mkdir +
//! remove_dir_all overhead is included in the measurement but is identical
//! for both arms, so the comparison stays fair.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sley_bench::{run_git, run_sley, unique_temp_dir};
use std::fs;
use std::path::Path;

type Runner = fn(&Path, &[&str], &[u8]) -> sley_core::Result<Vec<u8>>;

fn init_cycle(runner: Runner, label: &str) -> usize {
    let dir = unique_temp_dir("sley-bench-init");
    fs::create_dir_all(&dir).expect("create init target dir");
    let result = runner(&dir, &["init", "-q", "-b", "main", "."], &[]);
    let out = match result {
        Ok(body) => body.len(),
        Err(err) => panic!("{label} init failed: {err}"),
    };
    fs::remove_dir_all(&dir).expect("remove init target dir");
    out
}

fn init_fresh_dir(c: &mut Criterion) {
    let mut group = c.benchmark_group("init_fresh_dir");
    group.bench_function("sley", |b| {
        b.iter(|| black_box(init_cycle(run_sley, "sley")));
    });
    group.bench_function("git", |b| {
        b.iter(|| black_box(init_cycle(run_git, "git")));
    });
    group.finish();
}

criterion_group!(benches, init_fresh_dir);
criterion_main!(benches);
