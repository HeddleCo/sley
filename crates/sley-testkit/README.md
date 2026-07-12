# sley-testkit

Test harness utilities for sley parity and integration testing.

## Engine parity (`engine_parity`)

Library-first parity tests compare **`sley::Repository` (and related APIs)** against upstream **oracle git** subprocess output. Use this for the fast loop during CLI extraction; the full upstream script gate remains in `scripts/run-upstream-tests*.sh`.

### Environment

| Variable | Purpose |
|----------|---------|
| `SLEY_TEST_GIT` | Path to oracle git (default: `git` on `PATH`). Must be **2.55.x**. |
| `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` | Sealed to `/dev/null` (or `NUL` on Windows) for oracle subprocesses via [`hermetic_git_command`](src/lib.rs). |

Host `~/.gitconfig` does not affect oracle runs. Library-side config reads use on-disk repository files unless a test deliberately layers effective config.

### Quick start

```rust
use sley::Repository;
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase, git_bool_line};

EngineParityCase::new("rev-parse-is-shallow").run(
    |fixture| fixture.init_default(),
    |fixture| {
        let repo = Repository::discover(fixture.path()).unwrap();
        EngineOutput::stdout(git_bool_line(repo.is_shallow()))
    },
    |fixture| fixture.oracle(&["rev-parse", "--is-shallow-repository"]),
);
```

### API

- [`hermetic_repo`](src/engine_parity.rs) — temp directory fixture (RAII cleanup)
- [`EngineParityCase`](src/engine_parity.rs) — `{ name, setup, run_sley, run_oracle, compare }`
- [`assert_bytes_eq`](src/engine_parity.rs) / [`assert_stdout_eq`](src/engine_parity.rs) — byte-level diff helpers
- [`EngineOutput`](src/engine_parity.rs) — stdout / stderr / exit / optional file snapshots

Reference tests live in `crates/sley/tests/parity/`.

## CLI / upstream parity

- [`oracle_git`](src/lib.rs) — version-pinned oracle program path
- [`hermetic_git_command`](src/lib.rs) — subprocess builder with sealed config
- [`upstream`](src/lib.rs) — drive git's `t/*.sh` suite against the `sley` binary

[`upstream-manifest.tsv`](upstream-manifest.tsv) is the sole curated selection,
exclusion, prerequisite, platform/hash-applicability, and performance-eligibility
source. `scripts/run-upstream-tests.sh` runs a version-matched complete oracle
installation and Sley separately, classifies PASS/FAIL/TODO/SKIP/ABORT/TIMEOUT,
and emits exact per-cell comparisons. Sley's compatibility exec directory is
provenance-checked and may contain only Sley-owned adapters; pinned Git core
helpers that are not native are refused rather than borrowed from `PATH`.
Certification installs the oracle with `NO_TCLTK=YesPlease`. Optional Git GUI
prompt helpers otherwise change `git --list-cmds=main` and introduce two
known-broken `t1517` cells in Git v2.55.0; core, transport, and Scalar behavior
remain in the pinned oracle profile used for both correctness and timing.
Certification also runs `scripts/preflight_upstream_environment.py` before
either target. A host that forbids loopback or local IPC is invalid rather than
recording credential-cache, fsmonitor, ssh-agent, and protocol failures as
semantic mismatches.

Every serial or wave run writes a `*-metadata.tsv` identity sidecar recording
the candidate commit and binary checksum, upstream commit, manifest checksum,
platform/architecture, hash lane, target, version, and run label. Keep it with
the CSV artifacts: results without matching identity metadata must not be
combined into a parity or performance claim. Explicit manifest platform/hash
applicability is applied while resolving the curated list; `oracle` continues
to defer cell applicability to upstream Git's own prerequisite/SKIP rules.

## Upstream timing analysis

Use [`scripts/analyze_upstream_timings.py`](scripts/analyze_upstream_timings.py)
to compare an oracle run with a Sley run. It joins each run's timing and summary
CSVs, rejects incomplete or unequal work, and reports comparable performance
separately from correctness diagnostics. Optional normalized per-cell CSVs
upgrade the aggregate-count proxy to exact TAP-vector evidence.

```sh
python3 crates/sley-testkit/scripts/analyze_upstream_timings.py \
  --oracle-timings <git-timings.csv> \
  --oracle-summary <git-summary.csv> \
  --sley-timings <sley-timings.csv> \
  --sley-summary <sley-summary.csv> \
  --joined-csv <classification.csv> \
  --fail-on-measurable-gate
```

Failed, timed-out, aborted, incomplete-plan, and assertion-mismatched scripts
cannot appear as wins. See [`UPSTREAM_TIMINGS.md`](UPSTREAM_TIMINGS.md) for the
historical 891-script classification baseline, its superseded shell-launch
timings, the corrected direct-launch shard, and the exact comparison policy.

### Paired timing trials

[`scripts/run_paired_upstream_timings.py`](scripts/run_paired_upstream_timings.py)
drives the wave runner with isolated artifacts and alternating order. Nightly
mode runs three oracle/Sley pairs; certification mode runs five. It records the
actual wave wall time and calculates per-script medians only after every trial
has stable, exact TAP-cell vectors. The wall-time gate itself is withheld until
the entire selected suite is equal-work; stable oracle/Sley platform skips are
accepted only as matching non-work and never enter per-script speed metrics.
Partial parity cannot create a wall-clock win through early exits.

```sh
python3 crates/sley-testkit/scripts/run_paired_upstream_timings.py \
  --mode nightly \
  --output-dir /tmp/sley-paired-nightly \
  --git-src-dir /tmp/git-src \
  --oracle-bin /opt/git-2.55.0/bin/git \
  --sley-bin "$PWD/target/release/sley"

python3 crates/sley-testkit/scripts/run_paired_upstream_timings.py \
  --mode certification --fail-on-gate \
  --output-dir /tmp/sley-paired-certification \
  --git-src-dir /tmp/git-src \
  --oracle-bin /opt/git-2.55.0/bin/git \
  --sley-bin "$PWD/target/release/sley"
```

Use `--hash sha256` for the SHA-256 lane. `--dry-run` prints the exact command
and environment plan without creating the output directory or running tests.
The final directory contains `runs.csv`, `run-plan.json`, per-target raw
artifacts, `paired-median-comparison.csv`, and `paired-median-analysis.md`.
