# sley

An embeddable Rust Git-equivalent library with a thin, byte-compatible CLI
wrapper.

The pinned compatibility oracle is upstream Git 2.55.0. Repository semantics
live in typed engine crates and the `sley::Repository` facade; the CLI owns
argv/environment setup, terminal integration, dispatch, and Git-identical byte
rendering. The curated conformance harness runs the oracle and Sley separately
and compares exact TAP cells.

This repository is not a complete Git replacement yet. Tracked gaps are
explicit in [`PARITY.md`](PARITY.md).

## Build and test

```sh
cargo build -p sley-cli --bin sley --release
cargo test --workspace
./target/release/sley version
```

Run a command through the CLI:

```sh
cargo run -p sley-cli --release -- status --short
cargo run -p sley-cli --release -- log --oneline -20
```

## Upstream suite timing

The original 2026-07-10 Git v2.55.0 baseline ran 891 enrolled scripts on an
M1 Pro, but its Sley side used a generated `/bin/sh` shim for every command
while Git ran directly. That candidate-only process launch makes the historical
aggregate and tail timings unsuitable for release performance claims. Its
equal-work classification remains useful: only 425 scripts did demonstrably
equal work, and failing/short-circuiting rows remain excluded.

The harness now launches Sley directly under the installed `git` name. In the
first corrected three-pair shard, exact `t7004-tag.sh` (231/231 cells) measured
5.744 s for Git and 5.297 s for Sley: a 1.084× paired-median speedup, with the
selected-run aggregate, median, p95/p99, and wall-time comparisons all passing.
After reference-fsync policy and backend-selection caching, a five-script refs
shard retained exact work in three alternating trials and measured a 0.796
Sley/Git aggregate ratio, 1.265× median speedup, and 0.79 p95/p99 ratios; every
script beat Git, including `t1460-refs-migrate.sh` at 1.144×. After
repack/bitmap optimization, exact `t5333-pseudo-merge-bitmaps.sh` improved from
10.783 s to 8.081 s on Sley, but Git's 6.633 s median still leaves Sley at
0.821× and that shard's performance gates remain red. Exact
`t7063-status-untracked-cache.sh` is now 1.987× faster on Sley; exact
`t3311-notes-merge-fanout.sh` is 1.045× faster in the latest sample but
remains close enough to the 1.05× threshold to require the dedicated Linux
certification run.

The first clean-oracle, direct-launch full SHA-1 matrix completed all 891
scripts without an abort or timeout. Git v2.55.0 established 881 passing
scripts plus 10 legitimate skip-all scripts; Sley reported 496 passing, 385
failing, and the same 10 skip-all scripts. Exact per-cell comparison is the
release measure: 513/891 scripts match every oracle-applicable cell, while 378
remain incomparable. A one-shot equal-work diagnostic over the 496 scripts
that also passed end-to-end has Sley ahead in aggregate, median, p95, and p99,
but it is not an alternating paired run and is not a certification result.

The latest integrated eight-wave rerun raised the exact result to 556/891 and
the raw result to 538 passing, 343 failing, and 10 skip-all scripts
(30,103/32,357 assertions, 93%). A subsequently verified MIDX regression
repair (`t5319`, 98/98) and credential-engine closure (`t0300`, 56/56)
establish a current verified exact floor of 557/891.
`t0301-credential-cache.sh` was exact before its daemon-latency change, but
the managed sandbox now rejects its Unix socket bind, so it is not carried into
that floor until an unsandboxed rerun. On the 538 exact end-to-end rows in the
integrated run, the non-alternating equal-work diagnostic measured a 0.749
Sley/Git aggregate ratio, 1.227× median paired speedup, and 0.723/0.696 p95/p99
ratios. These are development diagnostics only: at least 334 correctness gaps
remain and the five-pair Linux certification has not run.

After caching repeated parent-directory metadata probes in status, a fresh
21-case, byte-identical common-command sample passes both gates: 0.811 Sley/Git
geometric mean and no row over 1.05. The former blocker, large-repository
`status --short`, now measures 22.87 ms for Sley versus 23.17 ms for Git.
Eight-wave wall time is still a separate, not-yet-certified measure.

This is targeted development evidence, not the required five-pair dedicated
Linux certification.
[`crates/sley-testkit/UPSTREAM_TIMINGS.md`](crates/sley-testkit/UPSTREAM_TIMINGS.md)
records both the superseded baseline and the corrected measurement protocol.

## Benchmark harnesses

| Harness | What it compares |
|---|---|
| `scripts/bench-human-common.py` | Timing, byte identity, and peak RSS for everyday porcelain |
| `scripts/bench-vs-git.sh` | Git and Sley on synthetic plumbing and pack fixtures |
| `crates/sley-testkit/scripts/run_paired_upstream_timings.py` | Alternating, exact-work upstream timing trials |
| `cargo bench -p sley-bench` | Internal library regression baselines |

Only byte-identical, equal-work rows are eligible for Git/Sley performance
claims. Re-run the local benchmark surfaces with:

```sh
python3 scripts/bench-human-common.py
./scripts/bench-vs-git.sh
cargo bench -p sley-bench
```

See the testkit timing document for the three-trial nightly and five-trial
Linux certification commands.

## Workspace layout

| Area | Crates (selection) |
|---|---|
| Core types / objects | `sley-core`, `sley-object`, `sley-formats` |
| Storage | `sley-odb`, `sley-pack`, `sley-index`, `sley-refs`, `sley-mmap` |
| History / worktree | `sley-rev`, `sley-worktree`, `sley-diff-merge`, `sley-unpack-trees` |
| Network | `sley-protocol`, `sley-remote`, `sley-transport` |
| Facade / CLI | `sley`, `sley-cli` |
| Parity / performance | `sley-testkit`, `sley-bench` |

## Documentation

| Document | Purpose |
|---|---|
| [`PARITY.md`](PARITY.md) | Implemented surface and known gaps |
| [`GOAL.md`](GOAL.md) | Long-term product goal |
| [`GIT_PARITY_CHECKLIST.md`](GIT_PARITY_CHECKLIST.md) | Phase checklist and release gates |
| [`TRACKER.md`](TRACKER.md) | Living engineering tracker |
| [`crates/sley-testkit/UPSTREAM_TIMINGS.md`](crates/sley-testkit/UPSTREAM_TIMINGS.md) | Exact-work timing protocol and evidence |
| [`docs/adr/`](docs/adr/) | Architecture decisions |

## License

Apache-2.0
