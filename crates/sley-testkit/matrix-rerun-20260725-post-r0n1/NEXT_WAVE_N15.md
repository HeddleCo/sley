# Wave N1.5 + near-miss harvest

**Base:** `32e5aedd` · banked EXACT **636/891** · tip includes sparse residual (t3602/t3705/t0020)  
**Goal:** maximize EXACT via N1 leftovers + 1-fail scripts.

## Agents

| Agent | Scripts | Priority cells |
|-------|---------|----------------|
| N1-close | t5516, t5510, t5548 | t5516#99 exact oid (EXACT if closed) |
| N1-proto | t5702, t1404 | packfile-uri cluster; D/F update-ref |
| Near-A | t1091, t1090, t2022, t2082, t3909, t4102, t4122, t4115 | 1-fail sparse/checkout/apply/stash |
| Near-B | t7408, t6432, t4213, t4206, t9304 | 1-fail log/merge/submodule/marks |

## Guards (every agent)

t0000-basic, t0020-crlf, t3602-rm-sparse-checkout, t5620-backfill (if present), t5516 once green.

## Build

```bash
cargo build --release -p sley-cli --bins
export SLEY_BIN=$PWD/target/release/sley
export SLEY_SCALAR_BIN=$PWD/target/release/scalar
export GIT_SRC_DIR=/tmp/git-src
export SLEY_DEFAULT_HASH=sha1
```

## Results (integrated tip, verified)

| Cluster | Wins |
|---------|------|
| N1-close | **t5516 128/128 PASS**, **t5548 25/25 PASS**, t5510 224/6 (+5) |
| N1-proto | **t1404 38/38 PASS** (restored EXACT), t5702 77/8 (+1, #58); packfile-uri residual |
| Near-A | **7/8 full PASS** (t1090, t1091, t2022, t3909, t4102, t4122, t4115); t2082 residual |
| Near-B | **5/5 full PASS** (t7408, t6432, t4213, t4206, t9304) |
| Guards | t0000, t0020, t3602, t3705 all PASS |

**Expected EXACT delta (smoke, not full matrix):** +~15–20 scripts vs 636 bank (incl. prior sparse residual t0020/t3602/t3705).
