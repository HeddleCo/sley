# Next waves (post exact-628 bank)

**Floor:** 628/891 exact · 608 raw PASS · 1178 SLEY_FAILURE cells · 262 gap scripts  
**Candidate base:** `fd46d208` / bank `6e1a72a2`  
**Goal trajectory:** restore lost EXACT → then high-gap near-miss clusters → remeasure.

---

## Wave R0 — restore lost EXACT (P0)

**Objective:** close the 10 CODE regressions from `LOST_EXACT_TRIAGE.md`.  
**Success:** all 10 scripts raw PASS + EXACT again → floor **~638**.

| Track | Scripts | Owner focus |
|-------|---------|-------------|
| R0-A options | t6014, t7102 | rev-list incompatible flags; reset `--refresh` |
| R0-B index/stat | t2002, t0000#80, t2007 | checkout-index smudge, diff-files, symlink↔dir |
| R0-C checkout DWIM | t2024 | multi-remote ambiguous branch fail |
| R0-D refs txn | t1400#192 | update-ref transaction status flush |
| R0-E push config | t5400, t5507 | denyDeletes / config must not travel local push |
| R0-F filter | t0021#17 | required process filter |

**Do not:** spend cycles on t1517 (HARNESS).

**Verify:** single-script smoke after each track; batch re-smoke all 10.

---

## Wave N1 — fetch/push near-miss mega-cluster

**Why:** largest remaining cell gaps; high ok% → easy EXACT wins.

| Script | Gap | ok% | Themes |
|--------|----:|----:|--------|
| t5510-fetch.sh | 22 | 90% | atomic fetch, prune D/F, bundles, short tags |
| t5516-fetch-push.sh | 16 | 87% | negotiation tip/restrict push, exact oid, allowtipsha1inwant, denyCurrentBranch |
| t5702-protocol-v2.sh | 16 | 81% | git://, custom path, ready DELIM/FLUSH, packfile-uri |
| t5548-push-porcelain.sh | 9 | 64% | porcelain push status |

**Agents (suggested 3):**

1. **fetch-atomic-prune-bundle** — t5510 focus (atomic transaction, prune, bundles)  
2. **fetch-push-oid-neg** — t5516 + t5548  
3. **proto-v2-uri-path** — t5702 residual (packfile-uri last; unborn/ls-refs already green)

**Expected:** +30–50 cells; possibly +3–5 exact scripts if t5510/t5516 close or near-close.

---

## Wave N2 — partial-clone / promisor residual

| Script | Gap | Notes |
|--------|----:|-------|
| t5616-partial-clone.sh | 11 | lazy .gitmodules, delta base hydrate, tag targets, pack-objects `--not` |
| t0410 / t4067 (if enrolled) | — | extend if in residual list |
| t5620 | 0 | already PASS — regression guard only |

**Agent:** promisor index-pack base fetch + no-checkout submodule .gitmodules hydrate.

---

## Wave N3 — commit/status/fsmonitor near-miss

| Script | Gap | ok% |
|--------|----:|----:|
| t7519-status-fsmonitor.sh | 15 | 51% |
| t7508-status.sh | 8 | 86% |
| t7512-status-help.sh | 8 | 83% |
| t9903-bash-prompt.sh | 13 | 80% |

**Agents:** status/fsmonitor hook path; prompt `__git_ps1` parity (often shell adapter).

---

## Wave N4 — diff/merge residual

| Script | Gap | Notes |
|--------|----:|-------|
| t4069-remerge-diff.sh | 12 | remerge headers incomplete (partial from prior) |
| t4048-diff-combined-binary.sh | 10 | combined binary |
| t7610-mergetool.sh | 10 | mergetool |
| t7003-filter-branch.sh | 12 | state-branch / tree-filter |

**Agents:** remerge-diff complete; combined-diff binary; filter-branch state.

---

## Wave N5 — pack / commit-graph / rev-list

| Script | Gap | Notes |
|--------|----:|-------|
| t5320-delta-islands.sh | 11 | islands |
| t5324-split-commit-graph.sh | 9 | continue from 103/110 |
| t5300-pack-object.sh | 8 | pack-objects edges |
| t6000-rev-list-misc.sh | 11 | misc options |
| t6600-test-reach.sh | 9 | reachability |

---

## Wave N6 — submodule residual

| Script | Gap |
|--------|----:|
| t7426-submodule-get-default-remote.sh | 11 |
| t7406-submodule-update.sh | 10 |
| t7425-submodule-gitdir-path-extension.sh | 10 |
| t7400-submodule-basic.sh | 8 |

Prior wave closed t2013/t3207/t7112 — keep unpack-trees/gitlink green as guardrails.

---

## Recommended sequencing

```
R0 (lost EXACT restore)     ← do first, small cells, protect floor
    ↓
N1 fetch/push/protocol      ← biggest gap ROI
    ↓ smoke key scripts
N2 promisor residual
N3 status/prompt            } can parallel after R0+N1
N4 diff/merge
N5 pack/rev
N6 submodule
    ↓
Full matrix remeasure (bank exact floor)
```

## Parallel fan-out template (after R0)

| Agent | Worktree isolation | Scripts | Stop condition |
|-------|--------------------|---------|----------------|
| R0-all or R0-A..F | worktree | lost EXACT 10 | all PASS |
| N1-fetch | worktree | t5510 | maximize ok, no regressed exact |
| N1-push | worktree | t5516, t5548 | maximize ok |
| N1-proto | worktree | t5702 | +cells, packfile-uri optional |
| N2-promisor | worktree | t5616 | +cells |
| N3-status | worktree | t7519, t7508, t7512 | +cells |
| N4-remerge | worktree | t4069, t4048 | +cells |
| N5-pack | worktree | t5324, t5320, t5300 | +cells |

**Guards every agent must re-smoke:** t5620, t5710, t5703, t7501, t7700, t5509, t5574, t6416.

## Floor checkpoints

| Checkpoint | Exact target | Trigger |
|------------|-------------:|---------|
| After R0 | ≥ 638 | 10 CODE restored |
| After N1 | ≥ 645 | fetch/push EXACT wins |
| After N1–N3 | ≥ 655 | status/protocol |
| Next full matrix | bank whatever | always remeasure before claiming floor |

## Metrics to report per agent

- Before/after ok and raw PASS/FAIL per script  
- Files changed  
- Residual cells with titles  
- Confirmation guards still PASS  
- No push/commit (orchestrator integrates)

---

## Quick-start commands

```bash
export GIT_SRC_DIR=/tmp/git-src
export SLEY_BIN=$PWD/target/release/sley   # or debug after rebuild
cargo build --release -p sley-cli --bins

# R0 batch
crates/sley-testkit/scripts/run-upstream-tests.sh \
  t0000-basic.sh t0021-conversion.sh t1400-update-ref.sh \
  t2002-checkout-cache-u.sh t2007-checkout-symlink.sh t2024-checkout-dwim.sh \
  t5400-send-pack.sh t5507-remote-environment.sh t6014-rev-list-all.sh t7102-reset.sh

# Guards
crates/sley-testkit/scripts/run-upstream-tests.sh \
  t5620-backfill.sh t5710-promisor-remote-capability.sh t5703-upload-pack-ref-in-want.sh \
  t7501-commit-basic-functionality.sh t7700-repack.sh t5509-fetch-push-namespaces.sh \
  t5574-fetch-output.sh t6416-recursive-corner-cases.sh
```
