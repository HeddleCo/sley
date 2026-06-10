//! sley-vs-git comparison for `config` reads (`--get`, `--list`,
//! `--get-regexp`) against a repo config with many `bench.*` keys.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sley_bench::{WorktreeBenchFixture, create_worktree_fixture, run_git, run_sley};
use std::path::Path;
use std::sync::OnceLock;

fn fixture() -> &'static WorktreeBenchFixture {
    static FIXTURE: OnceLock<WorktreeBenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_worktree_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("worktree fixture setup failed: {err}"),
    })
}

type Runner = fn(&Path, &[&str], &[u8]) -> sley_core::Result<Vec<u8>>;

fn compare(c: &mut Criterion, group_name: &str, args: &'static [&'static str]) {
    let fixture = fixture();
    let mut group = c.benchmark_group(group_name);
    let run = |runner: Runner, label: &str| match runner(&fixture.repo_root, args, &[]) {
        Ok(body) => black_box(body.len()),
        Err(err) => panic!("{label} {} failed: {err}", args.join(" ")),
    };
    group.bench_function("sley", |b| b.iter(|| run(run_sley, "sley")));
    group.bench_function("git", |b| b.iter(|| run(run_git, "git")));
    group.finish();
}

fn config_get(c: &mut Criterion) {
    compare(c, "config_get", &["config", "--get", "bench.key25"]);
}

fn config_list(c: &mut Criterion) {
    compare(c, "config_list", &["config", "--list"]);
}

fn config_get_regexp(c: &mut Criterion) {
    compare(
        c,
        "config_get_regexp",
        &["config", "--get-regexp", "^bench\\."],
    );
}

criterion_group!(benches, config_get, config_list, config_get_regexp);
criterion_main!(benches);
