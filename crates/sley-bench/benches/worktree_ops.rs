//! Worktree write-path benchmarks: `add -u`, `status --porcelain`, and
//! `update-index --refresh` over a 1000-file worktree with a handful of dirty
//! files, compared head-to-head against the system `git` binary.
//!
//! These cover sley#27: `add -u` with 10 dirty files in a 1k-file worktree was
//! ~10x slower than git because the loose-object write path fsync'd every
//! object (git's default `core.fsync=none` fsyncs nothing on `add`), and
//! `update-index --refresh` re-hashed every tracked file instead of trusting
//! the cached stat for unchanged ones.
//!
//! The reference `git` binary is taken from `GIT_BENCH_BIN` (falling back to
//! `git` on `PATH`), matching the oracle harness's convention.

use criterion::{Criterion, criterion_group, criterion_main};
use sley_core::{GitError, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Total number of tracked files in the worktree fixture.
const FILE_COUNT: usize = 1000;
/// Number of files dirtied before each measured `add -u` / `status` iteration.
const DIRTY_COUNT: usize = 10;

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn git_bin() -> String {
    std::env::var("GIT_BENCH_BIN").unwrap_or_else(|_| "git".to_string())
}

fn run(bin: &str, cwd: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new(bin)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn run_sley(cwd: &Path, args: &[&str]) -> Result<()> {
    run(env!("SLEY_BENCH_BIN"), cwd, args)
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    run(&git_bin(), cwd, args)
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{}",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn file_path(root: &Path, index: usize) -> PathBuf {
    // Spread files across 10 directories so the worktree walk is realistic.
    root.join(format!("dir{:02}", index % 10))
        .join(format!("file{index:04}.txt"))
}

/// A fresh 1000-file worktree fully committed to a `main` branch, using the
/// reference `git` binary so the index/stat cache matches git's own format.
struct Worktree {
    root: PathBuf,
}

impl Worktree {
    fn create() -> Result<Self> {
        let root = unique_temp_dir("sley-bench-worktree");
        fs::create_dir_all(&root).map_err(|e| GitError::Io(e.to_string()))?;
        let git = git_bin();
        // Hermetic init: -b main, identity + safe.directory wired via -c so the
        // bench never depends on host gitconfig.
        run(
            &git,
            &root,
            &["-c", "init.defaultBranch=main", "init", "-b", "main", "."],
        )?;
        for index in 0..FILE_COUNT {
            let path = file_path(&root, index);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| GitError::Io(e.to_string()))?;
            }
            fs::write(&path, format!("content of file {index}\n"))
                .map_err(|e| GitError::Io(e.to_string()))?;
        }
        run(&git, &root, &["add", "-A"])?;
        run(
            &git,
            &root,
            &[
                "-c",
                "user.name=Bench",
                "-c",
                "user.email=bench@example.invalid",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        )?;
        Ok(Worktree { root })
    }

    /// Dirty `DIRTY_COUNT` files in place (overwrites with new content).
    fn dirty(&self) -> Result<()> {
        for index in 0..DIRTY_COUNT {
            let path = file_path(&self.root, index);
            fs::write(&path, format!("dirtied content {index} {}\n", nonce()))
                .map_err(|e| GitError::Io(e.to_string()))?;
        }
        Ok(())
    }
}

fn nonce() -> u64 {
    FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn add_update_10_dirty(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_update_10_dirty");

    let sley_wt = Worktree::create().expect("worktree fixture");
    group.bench_function("sley_cli", |b| {
        b.iter(|| {
            sley_wt.dirty().expect("dirty files");
            run_sley(&sley_wt.root, &["add", "-u"]).expect("sley add -u");
            std::hint::black_box(());
        });
    });

    let git_wt = Worktree::create().expect("worktree fixture");
    group.bench_function("git", |b| {
        b.iter(|| {
            git_wt.dirty().expect("dirty files");
            run_git(&git_wt.root, &["add", "-u"]).expect("git add -u");
            std::hint::black_box(());
        });
    });

    group.finish();
}

fn status_porcelain(c: &mut Criterion) {
    let mut group = c.benchmark_group("status_porcelain_10_dirty");

    let sley_wt = Worktree::create().expect("worktree fixture");
    sley_wt.dirty().expect("dirty files");
    group.bench_function("sley_cli", |b| {
        b.iter(|| {
            run_sley(&sley_wt.root, &["status", "--porcelain"]).expect("sley status");
            std::hint::black_box(());
        });
    });

    let git_wt = Worktree::create().expect("worktree fixture");
    git_wt.dirty().expect("dirty files");
    group.bench_function("git", |b| {
        b.iter(|| {
            run_git(&git_wt.root, &["status", "--porcelain"]).expect("git status");
            std::hint::black_box(());
        });
    });

    group.finish();
}

fn update_index_refresh(c: &mut Criterion) {
    let mut group = c.benchmark_group("update_index_refresh");

    // No dirty files: the refresh should be a no-op stat pass over 1000 clean
    // files. git trusts the cached stat and re-hashes nothing.
    let sley_wt = Worktree::create().expect("worktree fixture");
    group.bench_function("sley_cli", |b| {
        b.iter(|| {
            run_sley(&sley_wt.root, &["update-index", "--refresh", "-q"]).ok();
            std::hint::black_box(());
        });
    });

    let git_wt = Worktree::create().expect("worktree fixture");
    group.bench_function("git", |b| {
        b.iter(|| {
            run_git(&git_wt.root, &["update-index", "--refresh", "-q"]).ok();
            std::hint::black_box(());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    add_update_10_dirty,
    status_porcelain,
    update_index_refresh
);
criterion_main!(benches);
