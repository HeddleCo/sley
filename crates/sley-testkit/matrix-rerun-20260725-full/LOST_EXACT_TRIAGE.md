# Lost EXACT triage (609 → 628 bank)

**Scope:** 11 scripts that were `EXACT`+`correctness=PASS` on matrix-rerun-20260723-full
and are no longer EXACT on matrix-rerun-20260725-full (`fd46d208`).

**Method:** old/new cell comparison + re-smoke with release `sley` (2026-07-25).

## Summary

| Class | Count | Action |
|-------|------:|--------|
| **CODE regression** (new oracle PASS / sley FAIL cell) | **10** | Fix in Wave R0 (priority) |
| **HARNESS / TODO drift** (no real FAIL; vector INCOMPARABLE) | **1** | Do not chase as CODE (`t1517`) |

Net: **all 10 raw FAIL losses are real single-cell CODE regressions** (each was 0 FAIL → 1 FAIL).
Re-smoke confirmed each failure is reproducible.

## Per-script classification

### CODE — fix in Wave R0 (one cell each)

| Script | Cell | Title | Likely area | Notes |
|--------|-----:|-------|-------------|-------|
| **t0000-basic.sh** | #80 | validate git diff-files output for a known cache/work tree state | `diff_files` / racy-clean / smudge | Cascades from checkout/index racy path used by basic suite |
| **t0021-conversion.sh** | #17 | required process filter should filter data | clean/smudge process filter | Regression in required filter failure path or filter driver |
| **t1400-update-ref.sh** | #192 | transaction flushes status updates | refs transaction / status | Transaction status flush ordering |
| **t2002-checkout-cache-u.sh** | #2 | without -u, git checkout-index smudges stat information | checkout-index / unpack-trees | Stat fields should stay zero/smudged without `-u` |
| **t2007-checkout-symlink.sh** | #2 | switch from symlink to dir | checkout / unpack-trees | Typechange symlink→dir safety |
| **t2024-checkout-dwim.sh** | #13 | checkout of branch from multiple remotes fails #2 | checkout DWIM | Ambiguous remote branch must fail (was OK before) |
| **t5400-send-pack.sh** | #8 | cannot override denyDeletes with git -c send-pack | push / receive-pack config | `-c` must not override receive.denyDeletes on send-pack path |
| **t5507-remote-environment.sh** | #4 | config does not travel over same-machine push | push env / local receive-pack | GIT_CONFIG / cmdline must not leak into local push child incorrectly; or must leak correctly for remote |
| **t6014-rev-list-all.sh** | #4 | rev-list --graph --no-walk is forbidden | rev-list option validation | Need hard reject of incompatible pair |
| **t7102-reset.sh** | #28 | --mixed --[no-]refresh sets refresh behavior | reset mixed / refresh | `--no-refresh` / refresh flag wiring on mixed reset |

**Common theme:** several touch **checkout/index stat materialization** and **config inheritance across local push** — likely collateral from sparse-checkout prefetch / sparse clone / `-c` export work in the last waves. Treat as a single “index-stat + config-boundary” cluster plus three small option-validation fixes.

### HARNESS — not CODE

| Script | Verdict |
|--------|---------|
| **t1517-outside-repo.sh** | Oracle **FAIL** this run; Sley raw FAIL only via known-breakage / TODO vs PASS **STATUS_MISMATCH** (36 TODOs flip-flop) + 6 missing trailing cells. Same class as prior “raw FAIL, exact-ish noise” scripts. **Exclude from CODE floor chases.** |

## Suggested fix order (R0)

1. **t6014** — pure option reject (`--graph` + `--no-walk`); tiny.
2. **t7102** — `--[no-]refresh` on mixed reset; local.
3. **t2002** — checkout-index without `-u` stat smudge (unlocks understanding of t0000 #80).
4. **t0000 #80** — often follows index/stat fix.
5. **t2007** — symlink↔dir (related to gitlink/typechange work).
6. **t2024** — DWIM multi-remote fail.
7. **t1400 #192** — transaction status flush.
8. **t5400 + t5507** — send-pack / local push config boundary (pair).
9. **t0021 #17** — process filter required path.

**Target:** restore all 10 CODE scripts to EXACT → theoretical floor **638/891** before any new gains.

## Re-smoke evidence

```
t0000-basic.sh               FAIL  91/1   #80
t0021-conversion.sh          FAIL  42/1   #17  (plan 43 vs matrix plan 36 — still 1 real fail)
t1400-update-ref.sh          FAIL 314/1   #192
t1517-outside-repo.sh        FAIL 332/36  known breakages only among remaining
t2002-checkout-cache-u.sh    FAIL   2/1   #2
t2007-checkout-symlink.sh    FAIL   3/1   #2
t2024-checkout-dwim.sh       FAIL  22/1   #13
t5400-send-pack.sh           FAIL  16/1   #8
t5507-remote-environment.sh  FAIL   4/1   #4
t6014-rev-list-all.sh        FAIL   3/1   #4
t7102-reset.sh               FAIL  37/1   #28
```
