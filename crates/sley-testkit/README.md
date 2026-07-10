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

See `scripts/run-upstream-tests.sh` and `upstream-report*.csv` for enrolled script subsets and history.

**Timing baseline (sley vs git on all 891 enrolled scripts):**
[`UPSTREAM_TIMINGS.md`](UPSTREAM_TIMINGS.md) and
`upstream-sley-vs-git-timings.csv` (raw: `upstream-timings-{sley,git}-20260710.csv`).