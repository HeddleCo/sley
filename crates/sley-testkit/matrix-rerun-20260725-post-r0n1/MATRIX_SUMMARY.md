# Full matrix re-measure 2026-07-25 post R0/N1

- **Candidate:** `6d144c6c` (R0 lost-EXACT restore + N1 fetch/push/protocol + unmerged-lanes audit)
- **Exact:** **636/891 (71.38%)** (prior bank **628** **+8** net; documented floor 557 **+79**)
- **Sley raw:** PASS **615** / FAIL **266** / SKIP **10**
- **Assertions:** **30981/32358 (95.59%** report `pass=95%`; exact fraction **95.59%**)
- **Oracle:** PASS **880** / SKIP **10** / FAIL **1** (`t1517-outside-repo.sh` — known harness/env)
- **Upstream:** git **2.55.0** · **hash** sha1 · **platform** macos/arm64
- **Waves:** 8 × 900s timeout · release `sley` + `scalar`

## Exact floor definition

`cell_vector=EXACT` **and** `correctness=PASS` in `upstream-cell-comparison-summary.csv`
(891 curated scripts, oracle-applicable cell vectors match).

Of the 636 exact scripts, **11** still report raw Sley `FAIL` with exact cell vectors
(known-breakage / harness noise — not semantic regressions):

| Script | Note |
|--------|------|
| t0450-txt-doc-vs-help.sh | help/doc harness |
| t1092-sparse-checkout-compatibility.sh | known-breakage vanished |
| t1410-reflog.sh | known-breakage noise |
| t3011-common-prefixes-and-directory-traversal.sh | harness |
| t3910-mac-os-precompose.sh | mac precompose |
| t4014-format-patch.sh | known-breakage |
| t4058-diff-duplicates.sh | harness |
| t4072-diff-max-depth.sh | harness |
| t5610-clone-detached.sh | known-breakage |
| t6437-submodule-merge.sh | known-breakage |
| t7528-signed-commit-ssh.sh | SSH signing env |

## Delta vs 2026-07-25 bank (628)

**+10 scripts restored EXACT** (R0 lost-EXACT restore):

| Script |
|--------|
| t0000-basic.sh |
| t0021-conversion.sh |
| t1400-update-ref.sh |
| t2002-checkout-cache-u.sh |
| t2007-checkout-symlink.sh |
| t2024-checkout-dwim.sh |
| t5400-send-pack.sh |
| t5507-remote-environment.sh |
| t6014-rev-list-all.sh |
| t7102-reset.sh |

**−2 scripts lost EXACT:**

| Script | Note |
|--------|------|
| t0020-crlf.sh | 35/36 — closed post-matrix by sparse residual agent (racy-clean + filters) |
| t1404-update-ref-errors.sh | 28/38 — still open (10 SLEY_FAILURE cells) |

Net: **628 + 10 − 2 = 636**.

## N1 residual peaks (post multi-way merge)

| Script | Pass/fail | Notes |
|--------|-----------|-------|
| t5510-fetch.sh | 219/11 | was ~222 isolated |
| t5516-fetch-push.sh | 127/1 | was ~128 isolated |
| t5702-protocol-v2.sh | 76/9 | was ~77 isolated |
| t5548-push-porcelain.sh | 21/4 | was ~25 isolated |

Merge still leaves isolated agent peaks slightly below peak — forward, not cherry-pick.

## Sparse residual (landed after this matrix)

Forward-ported in the same branch after this remeasure (not reflected in these CSVs):

| Script | Pre-matrix | Post-fix (agent verify) |
|--------|------------|--------------------------|
| t3602-rm-sparse-checkout.sh | 8/13 FAIL | **13/13 PASS** |
| t3705-add-sparse-checkout.sh | 18/20 FAIL | **20/20 PASS** |
| t0020-crlf.sh | 35/36 FAIL | **36/36 PASS** |

Expected next full bank: **≥637 EXACT** (t0020 restored) if no new losses; t3602/t3705 may also go EXACT if cell vectors match.

## Cell-level gaps

- **SLEY_FAILURE cells:** 1130 across 254 scripts (prior bank ~1178 / 262)
- **MATCH_PASS cells:** 30312

Top residual SLEY_FAILURE gaps (oracle PASS, sley FAIL):

| Gap cells | Script |
|----------:|--------|
| 15 | t7519-status-fsmonitor.sh |
| 13 | t9903-bash-prompt.sh |
| 12 | t7003-filter-branch.sh |
| 12 | t4069-remerge-diff.sh |
| 11 | t3202 / t7426 / t5510 / t5320 / t5616 / t6000 |
| 10 | t1404-update-ref-errors.sh (+ several others) |

## Unmerged remote lanes (`origin/parity/r6`–`r10`)

Audit on tip: leftover remote refs are **stale SHAs**; tip already at/above claimed floors.
Cherry-pick of r6–r10 not useful — residual work is **forward-port**, not merge of old branches.
See prior `matrix-rerun-20260725-full/UNMERGED_LANES_AUDIT.md`.

## Artifacts

- `upstream-oracle-*` / `upstream-sley-*`
- `upstream-cell-comparison.csv` + `upstream-cell-comparison-summary.csv`
- `run.log`
