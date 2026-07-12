# Upstream `t/*.sh` timing: Sley vs Git

> **Performance supersession notice:** the original 891-script run launched
> every Sley command through a generated `/bin/sh` shim while the oracle Git
> binary ran directly. Scripts with hundreds of commands therefore charged
> Sley hundreds of candidate-only process launches. The correctness/equal-work
> classifications below remain useful, but all historical duration ratios and
> gate verdicts are superseded. The harness now exposes Sley through a direct
> hardlink, symlink, or native copy named `git`.

The first corrected three-pair development shard is exact
`t7004-tag.sh` (231/231 cells): Git 5.744 s versus Sley 5.297 s at the
per-script median, a 1.084× speedup (Sley/Git 0.922). The selected-run wall
medians were 6.106 s and 5.891 s, respectively. All measurable shard gates
passed. This targeted macOS result does not replace the required full-suite,
five-pair dedicated Linux certification.

Further exact, direct-launch development shards show which historical deficits
were harness overhead and which remain engine work:

| script | cells | Git median | Sley median | Git/Sley | status |
|---|---:|---:|---:|---:|---|
| `t1460-refs-migrate.sh` | 37/37 | 12.222 s | 10.681 s | 1.144× | paired refs slice passes |
| `t5333-pseudo-merge-bitmaps.sh` | 24/24 | 6.633 s | 8.081 s | 0.821× | all shard gates fail |
| `t7063-status-untracked-cache.sh` | 58/58 | 12.806 s | 6.446 s | 1.987× | all per-script gates pass |
| `t3311-notes-merge-fanout.sh` | 24/24 | 11.820 s | 11.310 s | 1.045× | Sley faster; 0.5pp below 1.05× target |

The three-trial alternating refs/reftable slice closed the historical ref
long-tail while preserving exact vectors in every trial:

| script | cells | Git median | Sley median | Git/Sley |
|---|---:|---:|---:|---:|
| `t0610-reftable-basics.sh` | 91/91 | 13.606 s | 10.088 s | 1.349× |
| `t0613-reftable-write-options.sh` | 11/11 | 1.662 s | 1.196 s | 1.390× |
| `t1400-update-ref.sh` | 315/315 | 13.406 s | 10.594 s | 1.265× |
| `t1460-refs-migrate.sh` | 37/37 | 12.222 s | 10.681 s | 1.144× |
| `t0601-reffiles-pack-refs.sh` | 47/47 | 5.313 s | 4.205 s | 1.263× |

Across those five scripts, Sley/Git aggregate elapsed is 0.796, median paired
speedup is 1.265×, p95 and p99 ratios are 0.786 and 0.785, and selected-run
wall time is 11.59 s versus Git's 14.05 s. The engine change makes reference
durability follow Git v2.55's `core.fsync` component policy instead of forcing
two unconditional barriers per reftable transaction; backend selection is also
cached for migration and reflog loops.

The bitmap/repack slice reduced Sley's `t5333` median from 10.783 s to
8.081 s while holding exact work; the remaining deficit profiles primarily to
general revision walking, checkout, update-ref, fast-import, and cat-file.
These are three-pair, one-wave macOS development measurements, not Linux
certification results. `t3311` is load-sensitive: an earlier exact three-pair
set measured a 1.106× Sley speedup, so neither sample is treated as the final
certification verdict.

## Clean-oracle direct-launch correctness baseline

The integrated SHA-1 matrix was rerun in eight waves with a 900-second
per-script safety cap against a complete, version-matched Git v2.55.0
installation with native helper provenance enforced. The oracle
completed 881 scripts and emitted 10 legitimate skip-all plans. Sley completed
all 891 scripts without an abort or timeout: 496 passed, 385 failed, and the
same 10 skipped. Raw TAP was 30,047/32,357 (92%), but the stricter result is
513 exact per-cell script vectors and 378 incomparable vectors. Seventeen
scripts are exact for every oracle-applicable cell even though their raw script
status is non-passing because of TODO/SKIP structure; they are correctness
matches but not performance rows.

A single non-alternating comparison has 496 exact, end-to-end performance rows:

| metric | Git | Sley | diagnostic result |
|---|---:|---:|---:|
| sum of per-script elapsed | 1,768.21 s | 1,419.40 s | 1.246× |
| median paired speedup | — | — | 1.189× |
| p95 script elapsed | 14.45 s | 11.61 s | Sley/Git 0.803 |
| p99 script elapsed | 35.43 s | 26.09 s | Sley/Git 0.736 |

This is encouraging equal-work evidence, not a release performance verdict:
the runs were not paired or alternated, only 496/891 scripts are performance
comparable, and wall time was not captured by the paired driver. The blocking
numbers remain the three-pair nightly and five-pair dedicated-Linux
certification artifacts after correctness reaches exact parity.

## Latest integrated development checkpoint

The next eight-wave SHA-1 rerun completed all 891 scripts without an abort or
timeout: 538 passed, 343 failed, and 10 emitted the same skip-all plans as the
oracle. Raw TAP was 30,103/32,357 (93%); exact per-cell comparison was 556/891.
The run exposed one two-cell MIDX regression. A focused repair restored
`t5319-multi-pack-index.sh` to the oracle's exact 98/98 vector, and a subsequent
credential slice restored `t0300-credentials.sh` to exact 56/56. Those focused
oracle comparisons, plus a later environment-limited full refresh, establish a
current verified exact floor of 557/891. `t0301-credential-cache.sh` was exact
before the cache-daemon latency change, but its post-change run cannot bind its
Unix socket in the managed sandbox and is therefore not carried forward.

The later same-tree refresh is not a replacement baseline: sandbox policy also
prevented Git-daemon loopback startup in four scripts, rejected simple IPC, and
made an ssh-agent cell fail before Sley behavior was exercised. It recorded 554
exact vectors, four aborts, and zero timeouts. Carrying forward only prior exact
scripts whose owning code did not change yields the conservative 557 floor;
the next unsandboxed matrix must refresh all aggregate counts and validate
`t0301` directly.

The integrated run contains 538 exact, end-to-end performance rows:

| metric | Git | Sley | diagnostic result |
|---|---:|---:|---:|
| sum of per-script elapsed | 1,865.47 s | 1,396.76 s | 1.336× |
| median paired speedup | — | — | 1.227× |
| p95 script elapsed | 14.40 s | 10.41 s | Sley/Git 0.723 |
| p99 script elapsed | 37.66 s | 26.20 s | Sley/Git 0.696 |

This is still a single non-alternating, concurrently scheduled development
comparison. It excludes all unequal-work rows and passes the measurable
aggregate, median, p95, and p99 thresholds, but it does not establish the
eight-wave wall or common-command gates and does not replace five alternating
pairs on the dedicated Linux reference host.

Fresh isolated paired measurements also corrected two misleading full-suite
outliers. Exact `t6030-bisect-porcelain.sh` measured Git 13.682 s versus Sley
8.092 s (1.691×) across three alternating pairs. Exact
`t5327-multi-pack-bitmaps-rev.sh` measured 12.251 s versus 11.492 s (1.066×).
Exact `t5333-pseudo-merge-bitmaps.sh` remains a real regression at 5.534 s
versus 7.079 s (0.782×). Subcommand profiling attributes most of that gap to
fast-import, reference updates/listing, revision walking, and repeated process
startup; a pack-only ref-snapshot candidate was rejected after it made the
target median worse.

The common-command driver admitted all 21 Git/Sley CLI rows only after
byte-identical exit status, stdout, and stderr. An initial five-sample run
passed the aggregate gate at 0.809 Sley/Git but isolated one blocking row: the
large Git repository's `status --short` was 23.95 ms for Git and 29.42 ms for
Sley (1.228 Sley/Git). Status was repeating parent-directory metadata probes
for 4,775 files sharing roughly 223 directories. A worker-local prefix cache
reduced that tracked-status phase from about 9.5 ms to 4.8 ms.

The root-owned confirmation used 10 alternating samples per row. Its geometric
mean is 0.811 Sley/Git, all 21 rows satisfy the 1.05 per-case limit, and the
former blocker measures 23.17 ms for Git versus 22.87 ms for Sley. The
development artifact is `/tmp/sley-human-common-final-v2`; release evidence
must be archived from the dedicated Linux runner.

The superseded shell-shim baseline described below was measured on 2026-07-10
on macOS arm64 (Apple M1 Pro), using eight concurrent waves and a 240-second
per-script timeout for the 891 scripts enrolled from Git v2.55.0's `t/` suite.

The important result is the **equal-work comparison**, not the raw 891-script
ratio. Git and Sley completed different assertions in 466 scripts, so treating
every duration as a speed race would count early failures and skipped work as
performance wins.

## Historical equal-work result (shell-shim biased timing)

The current artifacts do not contain cell-level TAP records. The analyzer can
therefore certify only the strict aggregate proxy: both scripts reported
`PASS`, both completed their TAP plan, and `(ok, not-ok, total, plan-total)` is
identical. This produces 425 comparable scripts.

| metric | Git | Sley | paired result |
|---|---:|---:|---:|
| comparable scripts | 425 | 425 | aggregate-count proxy |
| sum of per-script elapsed | 1,662.28 s | 1,719.91 s | 0.966× |
| median paired speedup | — | — | **1.057×** |
| geometric-mean paired speedup | — | — | **1.058×** |
| p95 script elapsed | **14.55 s** | 15.57 s | — |
| p99 script elapsed | **27.01 s** | 48.96 s | — |

At the ±5% speedup threshold, Sley has 221 valid wins, 107 regressions,
and 97 scripts within the band. The typical comparable script is faster on
Sley, but Sley is 3.5% slower in aggregate and has a substantially slower p99.

Against the measurable release gates, only the median passed in this
superseded shell-shim run:

| gate | target | measured | status |
|---|---:|---:|---|
| Sley/Git aggregate elapsed | ≤0.950 | 1.035 | **FAIL** |
| median paired speedup | ≥1.050× | 1.057× | PASS |
| Sley/Git p95 elapsed | ≤1.000 | 1.070 | **FAIL** |
| Sley/Git p99 elapsed | ≤1.000 | 1.813 | **FAIL** |

Eight-wave wall time and common-command cells require separate artifacts and
are not inferred from the per-script CSVs.

The sums are sums of process elapsed times captured during concurrent waves.
They include wave contention and are **not a measured serial-suite duration**.

### Largest valid regressions by aggregate impact

| script | Git | Sley | delta | speedup |
|---|---:|---:|---:|---:|
| `t7004-tag.sh` | 14.07 s | 57.62 s | +43.55 s | 0.244× |
| `t1460-refs-migrate.sh` | 19.14 s | 57.53 s | +38.39 s | 0.333× |
| `t5333-pseudo-merge-bitmaps.sh` | 10.05 s | 32.93 s | +22.88 s | 0.305× |
| `t3404-rebase-interactive.sh` | 44.72 s | 57.17 s | +12.45 s | 0.782× |
| `t3311-notes-merge-fanout.sh` | 21.18 s | 31.75 s | +10.57 s | 0.667× |

The first two rows alone add 81.94 seconds, more than the net 57.63-second
deficit over all 425 comparable scripts. They are the highest-leverage initial
optimization targets.

### Largest valid wins by aggregate impact

| script | Git | Sley | time saved | speedup |
|---|---:|---:|---:|---:|
| `t5533-push-cas.sh` | 22.92 s | 14.89 s | 8.02 s | 1.539× |
| `t1450-fsck.sh` | 21.03 s | 15.33 s | 5.70 s | 1.372× |
| `t6500-gc.sh` | 14.29 s | 9.79 s | 4.49 s | 1.459× |
| `t5400-send-pack.sh` | 18.56 s | 14.52 s | 4.04 s | 1.278× |
| `t5517-push-mirror.sh` | 8.12 s | 5.00 s | 3.11 s | 1.621× |

Only equal-work rows are eligible for these tables. Apparent high speedups in
failing remote-helper, scalar, quickfetch, and fsmonitor scripts are excluded.

## Incomparable and all-run diagnostics

| reason | scripts |
|---|---:|
| Sley did not pass | 303 |
| neither implementation passed | 79 |
| oracle Git did not pass | 75 |
| aggregate TAP counts differed despite both passing | 9 |
| **total incomparable** | **466** |

The oracle's 154 non-passing scripts mean this source-build run is not a valid
release-certification oracle. The next baseline must use a complete Git prefix
with the matching built helpers. Until then, rows such as `t6120-describe` and
many submodule tests are correctness diagnostics, not timing comparisons.

For completeness, the raw workload totals were:

| diagnostic metric | Git | Sley |
|---|---:|---:|
| wall clock, eight waves | 12m 13s | 13m 13s |
| sum of per-script elapsed | 4,462.77 s | 4,788.40 s |
| scripts PASS / non-PASS | 737 / 154 | 509 / 382 |
| assertions reported passing | 93% | 92% |

These totals describe different work and must not be used to claim that either
implementation is faster.

## Reproduce the analysis

[`scripts/analyze_upstream_timings.py`](scripts/analyze_upstream_timings.py)
joins the timing and summary artifacts, validates that each run's files agree,
and emits Markdown or JSON plus an optional fully classified joined CSV:

```sh
python3 crates/sley-testkit/scripts/analyze_upstream_timings.py \
  --oracle-timings crates/sley-testkit/upstream-timings-git-20260710.csv \
  --oracle-summary crates/sley-testkit/upstream-summary-git-20260710.csv \
  --sley-timings crates/sley-testkit/upstream-timings-sley-20260710.csv \
  --sley-summary crates/sley-testkit/upstream-summary-sley-20260710.csv \
  --joined-csv /tmp/upstream-timing-classification.csv \
  --fail-on-measurable-gate
```

With `--fail-on-measurable-gate`, the command exits 1 if aggregate, median,
p95, or p99 misses the configured project target. Wall-time and common-command
gates still require their dedicated artifacts.

When the harness supplies normalized cell artifacts, pass both
`--oracle-cells` and `--sley-cells`. The accepted minimal schema is:

```csv
target,script,cell,status,raw_result,directive,description
oracle,t0001-init.sh,1,PASS,ok,,example passes
oracle,t0001-init.sh,2,TODO,not_ok,TODO,known breakage
oracle,t0001-init.sh,3,SKIP,ok,SKIP,missing prerequisite
```

Cell identities and normalized `pass`/`fail`/`TODO`/`SKIP` outcomes must match
exactly. A row with cell data on only one side is incomparable; the analyzer
does not silently downgrade partial evidence to the aggregate proxy.
Skip-all TAP plans may use `cell=plan,raw_result=plan`; that pseudo-cell is
compared but does not count toward the summary's assertion total. An oracle
non-skip cell that becomes a Sley skip is classified as an unexpected skip.

Run its tests with:

```sh
python3 -m unittest crates/sley-testkit/scripts/test_analyze_upstream_timings.py
```

## Certification protocol

The blocking performance baseline should be captured on a stable Linux host.
Alternate which implementation runs first, use three paired trials nightly and
five for release certification, then compare trial medians. macOS and Windows
remain useful non-blocking measurements. No row becomes performance-eligible
until both implementations complete the same oracle-applicable work.

The paired driver implements this protocol:

```sh
# Three alternating pairs; report without making a red nightly block mandatory.
python3 crates/sley-testkit/scripts/run_paired_upstream_timings.py \
  --mode nightly \
  --output-dir /tmp/sley-paired-nightly \
  --git-src-dir /tmp/git-src \
  --oracle-bin /opt/git-2.55.0/bin/git \
  --sley-bin "$PWD/target/release/sley"

# Five alternating pairs; exit 1 if an available release gate fails.
python3 crates/sley-testkit/scripts/run_paired_upstream_timings.py \
  --mode certification --fail-on-gate \
  --output-dir /tmp/sley-paired-certification \
  --git-src-dir /tmp/git-src \
  --oracle-bin /opt/git-2.55.0/bin/git \
  --sley-bin "$PWD/target/release/sley"
```

Odd trials run Git then Sley; even trials run Sley then Git. Each invocation
gets its own report, summary, timing, cell, detail, stdout, and stderr files.
The driver records measured wall time and exit status in `runs.csv`, then emits
a median equal-work report and joined classification. A script is excluded if
any trial is non-passing, cell-incomparable, incomplete, missing, or has a cell
vector that varies between trials. The eight-wave wall-time numbers remain
diagnostic, and their gate is `NOT MEASURABLE`, until every selected script has
stable equal-work evidence in every trial. Use `--hash sha256` for the SHA-256
lane.
