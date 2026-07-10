# Upstream `t/*.sh` timings: sley vs git

Measured 2026-07-10 on macOS arm64 (Apple M1 Pro), 8 parallel waves,
240s per-script timeout, enrolled **891** scripts from
`.github/workflows/upstream-parity.yml`.

| | sley | git (v2.55.0 source build) |
|--|------:|------:|
| binary | `target/release/sley` (`git-compat-i18n`) | `/tmp/git-src/git` (`NO_RUST=1`) |
| scripts PASS | 509 | 737 |
| scripts FAIL | 382 | 154 |
| assertions ok / fail | 29598 / 2332 | 29841 / 2102 |
| assertion pass % | 92% | 93% |
| wall clock (8 waves) | 13m 13s | 12m 13s |
| serial-equiv script time | 79.8 min | 74.4 min |
| median script | 1718 ms | 1917 ms |
| mean script | 5374 ms | 5009 ms |
| p90 / p99 | 12364 / 57169 ms | 11815 / 44159 ms |
| max | 194505 ms | 209262 ms |

**Speedup** = `git_ms / sley_ms` (>1 means sley is faster).

Across 891 paired scripts: **487** sley ≥5% faster, **242** sley ≥5% slower, **162** within ±5%.
Median speedup: **1.07×**; geometric-ish mean of ratios (fmean): **1.12×**.
Total serial time ratio (git/sley): **0.93×** (sley is 7.3% slower overall).

### Notes

- Both runs use the same harness (`run-upstream-tests-waves.sh`) and the same
  upstream `t/` from a v2.55.0 source tree. Git was built in-tree with `NO_RUST=1`
  so cargo workspace noise under `/tmp` is avoided; some git FAIL rows are
  environment/helper gaps (not a claim that stock git fails those scripts).
- Timing is wall time for each script process (includes setup/teardown).
- Parallel waves mean wall clock ≪ sum of script times.

## Costliest scripts (by sley time)

| script | git ms | sley ms | speedup | git | sley |
|--------|-------:|--------:|--------:|-----|------|
| `t1092-sparse-checkout-compatibility.sh` | 209262 | 194505 | 1.08× | FAIL | PASS |
| `t5510-fetch.sh` | 100403 | 89931 | 1.12× | FAIL | FAIL |
| `t0027-auto-crlf.sh` | 71902 | 85774 | 0.84× | PASS | FAIL |
| `t5310-pack-bitmaps.sh` | 26271 | 62806 | 0.42× | FAIL | FAIL |
| `t7112-reset-submodule.sh` | 24020 | 61189 | 0.39× | FAIL | FAIL |
| `t6120-describe.sh` | 11548 | 59969 | 0.19× | FAIL | PASS |
| `t7004-tag.sh` | 14071 | 57623 | 0.24× | PASS | PASS |
| `t1460-refs-migrate.sh` | 19142 | 57528 | 0.33× | PASS | PASS |
| `t3404-rebase-interactive.sh` | 44723 | 57169 | 0.78× | PASS | PASS |
| `t5572-pull-submodule.sh` | 16272 | 54722 | 0.30× | FAIL | FAIL |
| `t2013-checkout-submodule.sh` | 20658 | 52832 | 0.39× | FAIL | FAIL |
| `t5326-multi-pack-bitmaps.sh` | 37018 | 52618 | 0.70× | FAIL | FAIL |
| `t1800-hook.sh` | 51308 | 50703 | 1.01× | PASS | PASS |
| `t1013-read-tree-submodule.sh` | 21559 | 50301 | 0.43× | FAIL | PASS |
| `t3432-rebase-fast-forward.sh` | 44159 | 50196 | 0.88× | PASS | PASS |
| `t6423-merge-rename-directories.sh` | 54052 | 45654 | 1.18× | PASS | FAIL |
| `t3070-wildmatch.sh` | 40583 | 45026 | 0.90× | PASS | PASS |
| `t5500-fetch-pack.sh` | 37565 | 44336 | 0.85× | FAIL | FAIL |
| `t6438-submodule-directory-file-conflicts.sh` | 14618 | 42935 | 0.34× | FAIL | PASS |
| `t5516-fetch-push.sh` | 87161 | 42912 | 2.03× | FAIL | FAIL |

## Where sley is slowest vs git (largest absolute delta)

| script | git ms | sley ms | delta | speedup |
|--------|-------:|--------:|------:|--------:|
| `t6120-describe.sh` | 11548 | 59969 | +48421 | 0.19× |
| `t7004-tag.sh` | 14071 | 57623 | +43552 | 0.24× |
| `t5572-pull-submodule.sh` | 16272 | 54722 | +38450 | 0.30× |
| `t1460-refs-migrate.sh` | 19142 | 57528 | +38386 | 0.33× |
| `t7112-reset-submodule.sh` | 24020 | 61189 | +37169 | 0.39× |
| `t5310-pack-bitmaps.sh` | 26271 | 62806 | +36535 | 0.42× |
| `t2013-checkout-submodule.sh` | 20658 | 52832 | +32174 | 0.39× |
| `t5702-protocol-v2.sh` | 576 | 31712 | +31136 | 0.02× |
| `t1013-read-tree-submodule.sh` | 21559 | 50301 | +28742 | 0.43× |
| `t6438-submodule-directory-file-conflicts.sh` | 14618 | 42935 | +28317 | 0.34× |
| `t3426-rebase-submodule.sh` | 9223 | 37217 | +27994 | 0.25× |
| `t7003-filter-branch.sh` | 6832 | 33726 | +26894 | 0.20× |
| `t4255-am-submodule.sh` | 7829 | 33475 | +25646 | 0.23× |
| `t5333-pseudo-merge-bitmaps.sh` | 10049 | 32932 | +22883 | 0.31× |
| `t9301-fast-import-notes.sh` | 15583 | 34598 | +19015 | 0.45× |
| `t5326-multi-pack-bitmaps.sh` | 37018 | 52618 | +15600 | 0.70× |
| `t7406-submodule-update.sh` | 6221 | 21660 | +15439 | 0.29× |
| `t5526-fetch-submodules.sh` | 6811 | 22217 | +15406 | 0.31× |
| `t0027-auto-crlf.sh` | 71902 | 85774 | +13872 | 0.84× |
| `t4137-apply-submodule.sh` | 9241 | 22923 | +13682 | 0.40× |

## Where sley is fastest vs git (highest speedup)

| script | git ms | sley ms | speedup |
|--------|-------:|--------:|--------:|
| `t5801-remote-helpers.sh` | 12077 | 1622 | 7.45× |
| `t9210-scalar.sh` | 8543 | 1517 | 5.63× |
| `t5502-quickfetch.sh` | 4043 | 885 | 4.57× |
| `t7519-status-fsmonitor.sh` | 11781 | 2936 | 4.01× |
| `t3433-rebase-across-mode-change.sh` | 3390 | 929 | 3.65× |
| `t1517-outside-repo.sh` | 6596 | 1868 | 3.53× |
| `t9211-scalar-clone.sh` | 5486 | 1678 | 3.27× |
| `t6050-replace.sh` | 8942 | 2753 | 3.25× |
| `t6419-merge-ignorecase.sh` | 1452 | 492 | 2.95× |
| `t5504-fetch-receive-strict.sh` | 7191 | 2658 | 2.71× |
| `t5710-promisor-remote-capability.sh` | 12210 | 4655 | 2.62× |
| `t4111-apply-subdir.sh` | 3092 | 1210 | 2.56× |
| `t3504-cherry-pick-rerere.sh` | 2879 | 1172 | 2.46× |
| `t3452-history-split.sh` | 14422 | 5980 | 2.41× |
| `t3451-history-reword.sh` | 6835 | 2916 | 2.34× |

## Artifacts

| file | contents |
|------|----------|
| `upstream-sley-vs-git-timings.csv` | joined per-script comparison |
| `upstream-timings-sley-20260710.csv` | raw sley timings |
| `upstream-timings-git-20260710.csv` | raw git timings |
| `upstream-summary-sley-20260710.csv` | sley pass/fail assertion counts |
| `upstream-summary-git-20260710.csv` | git pass/fail assertion counts |
