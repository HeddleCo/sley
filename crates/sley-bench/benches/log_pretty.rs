//! sley-vs-git comparison for `log` pretty formats over a 1k-commit history.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sley_bench::{CommitBenchFixture, create_commit_fixture, run_git, run_sley};
use std::path::Path;
use std::sync::OnceLock;

fn fixture() -> &'static CommitBenchFixture {
    static FIXTURE: OnceLock<CommitBenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_commit_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("commit fixture setup failed: {err}"),
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

fn log_oneline_200(c: &mut Criterion) {
    compare(c, "log_oneline_200", &["log", "--oneline", "-n", "200"]);
}

fn log_format_200(c: &mut Criterion) {
    compare(
        c,
        "log_format_200",
        &["log", "--pretty=format:%H %an %ad %s", "-n", "200"],
    );
}

fn log_oneline_full(c: &mut Criterion) {
    compare(c, "log_oneline_full_history", &["log", "--oneline"]);
}

criterion_group!(benches, log_oneline_200, log_format_200, log_oneline_full);
criterion_main!(benches);
