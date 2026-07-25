# Full matrix bank 2026-07-25 post N1.5+N2

- **Candidate:** `e8246fed` (N1.5 residual harvest + N2 partial-clone/promisor + near-miss C)
- **Exact:** **656/891 (73.63%)** (prior post-R0/N1 bank **636** **+20** net)
- **Sley raw:** PASS **636** / FAIL **245** / SKIP **10**
- **Assertions:** **31038/32358 (95.92%)** (prior 30981 / 95.59%)
- **Oracle:** PASS **880** / SKIP **10** / FAIL **1** · **git 2.55.0** via `/opt/homebrew/bin/git`
- **Upstream:** `e9019fca` (git 2.55.0) · **hash** sha1 · **platform** macos/arm64
- **Waves:** 8 × 900s · release `sley` + `scalar`

## Bank validity note

First oracle attempt used Apple git **2.50.1** and aborted (0 scripts).  
**Oracle was re-run** with `SLEY_ORACLE_BIN=/opt/homebrew/bin/git` (2.55.0) and cell
comparison rebuilt against the completed sley artifacts. This summary is the
authoritative floor for `e8246fed`.

## Exact floor definition

`cell_vector=EXACT` **and** `correctness=PASS` in `upstream-cell-comparison-summary.csv`.

Of the 656 exact scripts, **10** still report raw Sley `FAIL` with exact cell vectors
(known-breakage / harness noise):

| Script |
|--------|
| t0450-txt-doc-vs-help.sh |
| t1410-reflog.sh |
| t3011-common-prefixes-and-directory-traversal.sh |
| t3910-mac-os-precompose.sh |
| t4014-format-patch.sh |
| t4058-diff-duplicates.sh |
| t4072-diff-max-depth.sh |
| t5610-clone-detached.sh |
| t6437-submodule-merge.sh |
| t7528-signed-commit-ssh.sh |

## Delta vs post-R0/N1 bank (636)

**+27 newly EXACT** (includes sparse residual, N1.5, N2, near-miss harvest):

| Script | Wave |
|--------|------|
| t0020-crlf, t3602, t3705 | sparse residual |
| t5516, t5548, t1404 | N1.5 residual |
| t1090, t1091, t2022, t3909, t4102, t4110, t4115, t4122, t4134, t3702 | near-miss A/C |
| t4206, t4213, t6432, t7408, t9304 | near-miss B |
| t0410, t4067, t1022, t5330 | N2 promisor/diff |
| t4109-apply-multifrag | bonus EXACT |

**−7 lost EXACT:**

- t0021-conversion.sh, t1092-sparse-checkout-compatibility.sh, t1501-work-tree.sh
- t4103-apply-binary.sh, t4112-apply-renames.sh, t4114-apply-typechange.sh
- t6403-merge-file.sh

Net: **636 + 27 − 7 = 656**.

## Cell-level gaps

- **SLEY_FAILURE cells:** 1073 across 234 scripts (was 1130 / 254 at post-R0/N1)
- Top residuals: t7519 (15), t9903 (13), t7003/t4069 (12), t3202/t7426/t5320/t6000 (11)

## Residual N2

| Script | Status |
|--------|--------|
| t5616-partial-clone.sh | 38/9 FAIL (not EXACT) |

## Artifacts

- `upstream-oracle-*` (retry with homebrew git 2.55.0)
- `upstream-sley-*` (from full sley run on e8246fed)
- `upstream-cell-comparison.csv` + `upstream-cell-comparison-summary.csv` (rebuilt)
- `run.log` (sley) · `run-oracle-retry.log` (oracle fix)
