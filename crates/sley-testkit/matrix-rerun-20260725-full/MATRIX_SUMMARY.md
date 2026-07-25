# Full matrix re-measure 2026-07-25

- **Candidate:** `fd46d208` (wave landings through commit/want-ref/repack + sparse backfill)
- **Exact:** **628/891** (prior full matrix 609 **+19**; documented floor 557 **+71**)
- **Sley raw:** PASS **608** / FAIL **273** / SKIP **10**
- **Assertions:** **30932/32358 (95.59%)** (prior ~95.01% / 93%)
- **Oracle:** PASS **880** / SKIP **10** / FAIL **1** (`t1517-outside-repo.sh` — known harness/env)
- **Upstream:** `e9019fca` (git 2.55.0) · **hash** sha1 · **platform** macos/arm64
- **Waves:** 8 × 900s timeout · release `sley` + `scalar`

## Exact floor definition

`cell_vector=EXACT` **and** `correctness=PASS` in `upstream-cell-comparison-summary.csv`
(891 curated scripts, oracle-applicable cell vectors match).

Of the 628 exact scripts, **10** still report raw Sley `FAIL` with exact cell vectors
(known-breakage / harness noise — not semantic regressions):

| Script | Note |
|--------|------|
| t0450-txt-doc-vs-help.sh | help/doc harness |
| t1092-sparse-checkout-compatibility.sh | known-breakage vanished |
| t1410-reflog.sh | known-breakage noise |
| t3011-common-prefixes-and-directory-traversal.sh | harness |
| t4014-format-patch.sh | known-breakage |
| t4058-diff-duplicates.sh | harness |
| t4072-diff-max-depth.sh | harness |
| t5610-clone-detached.sh | known-breakage |
| t6437-submodule-merge.sh | known-breakage |
| t7528-signed-commit-ssh.sh | SSH signing env |

## Gains vs 2026-07-23 floor (609)

**+30 scripts newly EXACT** (net **+19** after 11 losses):

Notable closes this wave banked into exact:

| Script | Theme |
|--------|--------|
| t5620-backfill.sh | native backfill + sparse partial clone |
| t5710-promisor-remote-capability.sh | promisor connectivity |
| t5703-upload-pack-ref-in-want.sh | want-ref / ERR |
| t7501-commit-basic-functionality.sh | commit porcelain |
| t7700-repack.sh | repack/filter/promisor loosen |
| t5509 / t5574 | namespaces + fetch output |
| t0001 / t0020 / t0600 / t2200 | near-miss harvest A |
| t4124 / t6006 / t5334 / t6120 | near-miss harvest B |
| t2013 / t3207 / t7112 | submodule gitlink cluster |
| t6416-recursive-corner-cases.sh | merge virtual-ancestor |
| t1305 / t4210 / t4255 | prior hard CODE closes |

**−11 scripts lost EXACT** (investigate if real regressions):

- t0000-basic.sh, t0021-conversion.sh, t1400-update-ref.sh
- t1517-outside-repo.sh (oracle also FAIL this run)
- t2002-checkout-cache-u.sh, t2007-checkout-symlink.sh, t2024-checkout-dwim.sh
- t5400-send-pack.sh, t5507-remote-environment.sh
- t6014-rev-list-all.sh, t7102-reset.sh

## Top remaining cell gaps (oracle PASS, sley FAIL)

| Gap cells | Script |
|----------:|--------|
| 22 | t5510-fetch.sh |
| 16 | t5702-protocol-v2.sh |
| 16 | t5516-fetch-push.sh |
| 15 | t7519-status-fsmonitor.sh |
| 13 | t9903-bash-prompt.sh |
| 12 | t7003-filter-branch.sh |
| 12 | t4069-remerge-diff.sh |
| 11 | t3202 / t5616 / t6000 / … |

**Total SLEY_FAILURE cells:** 1178 across 262 scripts (was ~1371 / 278 at prior matrix).

## Artifacts

All under `crates/sley-testkit/matrix-rerun-20260725-full/`:

- `upstream-oracle-*.{csv,tsv,txt}`
- `upstream-sley-*.{csv,tsv,txt}`
- `upstream-cell-comparison.csv` + `-summary.csv`
- `run.log`
