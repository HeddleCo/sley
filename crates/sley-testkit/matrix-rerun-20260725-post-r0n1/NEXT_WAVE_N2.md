# Wave N2 — partial-clone / promisor + residual 1-fail

**Base:** `a67679dd` (N1.5 harvest) · banked EXACT still **636** (full matrix `matrix-rerun-20260725-post-n15` in flight for N1.5 floor)

## Agents

| Agent | Scripts |
|-------|---------|
| N2-partial | t5616-partial-clone (11 fail), t0410-partial-clone (3 fail) |
| N2-diff-promisor | t4067-diff-partial-clone (8 fail), t5330-no-lazy-fetch-with-commit-graph (1 fail) |
| Near-C | remaining 1-fail: t1022, t3702, t4110, t4134, t6134, t6428, t8009, t2082 |

## Guards

t5620-backfill, t5710-promisor-remote-capability, t0000, t0020, t5516, t3602

## Build

```bash
cargo build --release -p sley-cli --bins
export SLEY_BIN=$PWD/target/release/sley SLEY_SCALAR_BIN=$PWD/target/release/scalar
export GIT_SRC_DIR=/tmp/git-src SLEY_DEFAULT_HASH=sha1
```

## Results (integrated tip, verified)

| Cluster | Wins |
|---------|------|
| N2-partial | **t0410 39/39 PASS**; t5616 36→**38**/9 (+2) |
| N2-diff-promisor | **t4067 9/9 PASS**, **t5330 4/4 PASS**, **t1022 1/1 PASS** |
| Near-C | **4 full PASS**: t3702, t4110, t6134, t4134 |
| Guards | t5620, t5710, t0000, t0020, t5516 all PASS |

Residual: t5616 9 cells (gitmodules hydrate, delta-base, submodule lazy-fetch, tag target, REF_DELTA).
