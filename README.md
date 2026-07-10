# sley

Pure-Rust, minimal-dependency Git-compatible implementation. Compatibility
target is upstream **Git 2.55.0**.

sley is a greenfield workspace of typed library crates plus a `sley` CLI that
aims for byte- and behavior-compatible plumbing/porcelain. It is not a complete
drop-in Git replacement yet — tracked gaps live in [`PARITY.md`](PARITY.md).

## Build & test

```sh
cargo build -p sley-cli --bin sley --release
cargo test --workspace
./target/release/sley version   # reports as git version 2.55.0 for compatibility
```

Run a command through the CLI:

```sh
cargo run -p sley-cli --release -- status --short
cargo run -p sley-cli --release -- log --oneline -20
```

## Benchmarks vs git

sley is measured against system/source git on the same machine:

| Harness | Script | What it compares |
|---------|--------|------------------|
| Human common commands | `scripts/bench-human-common.py` | Timing + peak RSS for everyday porcelain |
| Plumbing / pack fixtures | `scripts/bench-vs-git.sh` | Hyperfine means on synthetic pack/commit fixtures |
| Upstream `t/*.sh` suite | `crates/sley-testkit/scripts/run-upstream-tests-waves.sh` | Wall time for all **891** enrolled scripts (sley and git) |
| Criterion (library) | `cargo bench -p sley-bench` | Internal regression baselines (no git oracle) |

### Environment (latest run)

| Field | Value |
|-------|-------|
| Date | 2026-07-10 |
| Platform | macOS arm64 (Apple M1 Pro) |
| git | 2.55.0 (`/opt/homebrew/bin/git`) |
| sley | `target/release/sley` (workspace HEAD) |
| Warmup / runs | 3 warmup, 12 timed (timing); 3 RSS samples (memory) |

**Modes**

- **git** — system git process (baseline)
- **sley_cli** — release `sley` process, including process startup (fair CLI comparison)
- **sley_harness** — in-process library path via `sley-human-harness` (library-use bound)

**Repositories**

| size | repo | commits | tracked files |
|------|------|--------:|--------------:|
| sm | [walkdir](https://github.com/BurntSushi/walkdir) | 192 | 20 |
| md | [ripgrep](https://github.com/BurntSushi/ripgrep) | 2,252 | 222 |
| lg | [git](https://github.com/git/git) | 81,471 | 4,775 |

### Timing (mean ms) — sley_cli vs git

Speedup `> 1` means sley is faster. Full three-mode tables (including harness) are
regenerated under `bench-results/human-common/<timestamp>/` when you re-run.

| repo | command | git | sley | speedup |
|------|---------|----:|-----:|--------:|
| sm/walkdir | `status --short` | 9.65 | 6.48 | **1.49×** |
| sm/walkdir | `log --oneline -100` | 9.14 | 7.01 | **1.30×** |
| sm/walkdir | `branch --list` | 8.48 | 5.85 | **1.45×** |
| sm/walkdir | `tag --list` | 9.03 | 5.78 | **1.56×** |
| sm/walkdir | `rev-parse --short HEAD` | 8.44 | 5.86 | **1.44×** |
| sm/walkdir | `branch -f … HEAD` | 9.11 | 6.93 | **1.32×** |
| sm/walkdir | `tag -f … HEAD` | 9.72 | 6.37 | **1.53×** |
| md/ripgrep | `status --short` | 11.16 | 9.07 | **1.23×** |
| md/ripgrep | `log --oneline -100` | 9.82 | 6.87 | **1.43×** |
| md/ripgrep | `branch --list` | 8.71 | 6.05 | **1.44×** |
| md/ripgrep | `tag --list` | 8.87 | 6.00 | **1.48×** |
| md/ripgrep | `rev-parse --short HEAD` | 8.63 | 6.04 | **1.43×** |
| md/ripgrep | `branch -f … HEAD` | 9.36 | 6.96 | **1.34×** |
| md/ripgrep | `tag -f … HEAD` | 10.25 | 6.91 | **1.48×** |
| lg/git | `status --short` | 25.54 | 26.99 | 0.95× |
| lg/git | `log --oneline -100` | 13.70 | 7.72 | **1.77×** |
| lg/git | `branch --list` | 9.03 | 6.39 | **1.41×** |
| lg/git | `tag --list` | 9.02 | 6.85 | **1.32×** |
| lg/git | `rev-parse --short HEAD` | 8.81 | 6.18 | **1.43×** |
| lg/git | `branch -f … HEAD` | 15.44 | 8.29 | **1.86×** |
| lg/git | `tag -f … HEAD` | 11.98 | 6.91 | **1.73×** |

In-process harness (no process startup) is much faster on small commands — e.g.
lg `rev-parse --short HEAD` is ~0.13 ms vs git’s 8.81 ms (~68×), and sm
`tag --list` is ~0.08 ms vs 9.03 ms (~112×). Use that column when judging library
embedding cost rather than CLI process cost.

### Memory (mean peak RSS, MiB) — sley_cli vs git

Ratio `< 1` means sley uses less resident memory.

| repo | command | git | sley | ratio |
|------|---------|----:|-----:|------:|
| sm/walkdir | `status --short` | 8.16 | 5.03 | **0.62×** |
| sm/walkdir | `log --oneline -100` | 7.85 | 4.99 | **0.64×** |
| sm/walkdir | `branch --list` | 7.14 | 3.59 | **0.50×** |
| sm/walkdir | `tag --list` | 7.13 | 3.38 | **0.47×** |
| sm/walkdir | `rev-parse --short HEAD` | 7.19 | 3.57 | **0.50×** |
| sm/walkdir | `branch -f … HEAD` | 7.59 | 4.69 | **0.62×** |
| sm/walkdir | `tag -f … HEAD` | 7.84 | 3.62 | **0.46×** |
| md/ripgrep | `status --short` | 8.42 | 5.36 | **0.64×** |
| md/ripgrep | `log --oneline -100` | 8.21 | 5.39 | **0.66×** |
| md/ripgrep | `branch --list` | 7.15 | 3.62 | **0.51×** |
| md/ripgrep | `tag --list` | 7.16 | 3.71 | **0.52×** |
| md/ripgrep | `rev-parse --short HEAD` | 7.21 | 3.58 | **0.50×** |
| md/ripgrep | `branch -f … HEAD` | 7.65 | 4.83 | **0.63×** |
| md/ripgrep | `tag -f … HEAD` | 7.88 | 3.83 | **0.49×** |
| lg/git | `status --short` | 9.84 | 9.53 | **0.97×** |
| lg/git | `log --oneline -100` | 12.38 | 10.94 | **0.88×** |
| lg/git | `branch --list` | 7.18 | 3.72 | **0.52×** |
| lg/git | `tag --list` | 7.26 | 4.53 | **0.62×** |
| lg/git | `rev-parse --short HEAD` | 7.25 | 3.58 | **0.49×** |
| lg/git | `branch -f … HEAD` | 7.71 | 5.89 | **0.76×** |
| lg/git | `tag -f … HEAD` | 7.92 | 4.11 | **0.52×** |

Across this matrix, the release CLI is typically **~1.3–1.8× faster** than git and
uses **~0.5–0.65×** the peak RSS on small/medium repos. On the large git.git
worktree, `status --short` is within a few percent of git on both time and memory;
ref and short-history commands stay ahead.

### Plumbing fixtures (hyperfine)

Synthetic pack (500 objects) and commit-graph fixtures from
`scripts/bench-vs-git.sh` (warmup 5, ≥10 runs):

| command | git mean | sley mean | speedup |
|---------|---------:|----------:|--------:|
| `cat-file -p` (1 oid) | 7.3 ms | 4.1 ms | **1.78×** |
| `cat-file --batch-check` (500) | 8.7 ms | 5.8 ms | **1.49×** |
| `cat-file --batch` (500) | 13.4 ms | 8.3 ms | **1.62×** |
| `rev-parse` loop (500 oids) | 3.46 s | 2.38 s | **1.45×** |
| `count-objects -v` | 6.9 ms | 3.6 ms | **1.92×** |
| `rev-list --count HEAD` | 7.2 ms | 4.8 ms | **1.52×** |
| `for-each-ref` | 8.1 ms | 5.1 ms | **1.57×** |
| `ls-tree -r HEAD` | 7.1 ms | 4.4 ms | **1.62×** |

### Re-run locally

```sh
# Human commands: timing + memory vs git (clones walkdir/ripgrep/git under /tmp)
python3 scripts/bench-human-common.py
# optional: --sizes sm,md  --skip-memory  --repeat 12

# Plumbing hyperfine suite (requires hyperfine)
./scripts/bench-vs-git.sh

# Criterion library benches + baselines
cargo bench -p sley-bench
# see crates/sley-bench/baselines/
```

Results land in `bench-results/human-common/<timestamp>/` (`timing-summary.csv`,
`memory-summary.csv`, `README.md`). Refresh the tables above from that run when
you change hot paths.

### Upstream `t/*.sh` suite (891 scripts) vs git

Full enrolled parity subset from `.github/workflows/upstream-parity.yml`, run
through the same waves harness against **sley** and against a **v2.55.0 source
build of git** (`/tmp/git-src/git`). Full tables and CSVs:
[`crates/sley-testkit/UPSTREAM_TIMINGS.md`](crates/sley-testkit/UPSTREAM_TIMINGS.md).

| metric | sley | git |
|--------|-----:|----:|
| wall clock (8 waves) | 13m 13s | 12m 13s |
| serial-equiv script time | 79.8 min | 74.4 min |
| median script | **1.72 s** | 1.92 s |
| mean script | 5.37 s | 5.01 s |
| p90 / p99 | 12.4 / 57.2 s | 11.8 / 44.2 s |
| scripts PASS / FAIL | 509 / 382 | 737 / 154 |
| assertion pass rate | 92% | 93% |

**Speedup** = `git_ms / sley_ms` (>1 ⇒ sley faster). Across 891 paired scripts:
**487** sley ≥5% faster, **242** sley ≥5% slower, **162** within ±5%; median
speedup **1.07×**. Overall serial time is ~7% higher for sley because a long
tail (describe/tag/submodule/bitmaps) is much slower even though the median is
ahead.

| costliest (sley time) | git | sley | speedup |
|-----------------------|----:|-----:|--------:|
| `t1092-sparse-checkout-compatibility.sh` | 209.3s | 194.5s | **1.08×** |
| `t5510-fetch.sh` | 100.4s | 89.9s | **1.12×** |
| `t0027-auto-crlf.sh` | 71.9s | 85.8s | 0.84× |
| `t5310-pack-bitmaps.sh` | 26.3s | 62.8s | 0.42× |
| `t6120-describe.sh` | 11.5s | 60.0s | 0.19× |
| `t7004-tag.sh` | 14.1s | 57.6s | 0.24× |

| sley wins (highest speedup) | git | sley | speedup |
|-----------------------------|----:|-----:|--------:|
| `t5801-remote-helpers.sh` | 12.1s | 1.6s | **7.45×** |
| `t9210-scalar.sh` | 8.5s | 1.5s | **5.63×** |
| `t5502-quickfetch.sh` | 4.0s | 0.9s | **4.57×** |
| `t7519-status-fsmonitor.sh` | 11.8s | 2.9s | **4.01×** |

```sh
# Re-run enrolled 891 scripts against sley (needs built git source tree)
export GIT_SRC_DIR=/tmp/git-src
export SLEY_BIN=$PWD/target/release/sley
export SLEY_TESTS="$(scripts/extract-sley-tests-from-workflow.sh | tr '\n' ' ')"
export SLEY_UPSTREAM_WAVES=8 SLEY_TEST_TIMEOUT=240
crates/sley-testkit/scripts/run-upstream-tests-waves.sh

# Same suite against real git (baseline): set SLEY_BIN to the git binary
export SLEY_BIN=/tmp/git-src/git
crates/sley-testkit/scripts/run-upstream-tests-waves.sh
```

## Workspace layout

| Area | Crates (selection) |
|------|--------------------|
| Core types / objects | `sley-core`, `sley-object`, `sley-formats` |
| Storage | `sley-odb`, `sley-pack`, `sley-index`, `sley-refs`, `sley-mmap` |
| History / worktree | `sley-rev`, `sley-worktree`, `sley-diff-merge`, `sley-unpack-trees` |
| Network | `sley-protocol`, `sley-remote`, `sley-transport` |
| Facade / CLI | `sley`, `sley-cli` |
| Parity / perf | `sley-testkit`, `sley-bench` |

## Docs

| Doc | Purpose |
|-----|---------|
| [`PARITY.md`](PARITY.md) | Implemented surface and known gaps |
| [`GOAL.md`](GOAL.md) | Long-term product goal |
| [`GIT_PARITY_CHECKLIST.md`](GIT_PARITY_CHECKLIST.md) | Phase checklist including performance gates |
| [`TRACKER.md`](TRACKER.md) | Living engineering tracker |
| [`crates/sley-testkit/UPSTREAM_TIMINGS.md`](crates/sley-testkit/UPSTREAM_TIMINGS.md) | 891-script sley vs git timing baseline |
| [`docs/adr/`](docs/adr/) | Architecture decisions |

## License

Apache-2.0
