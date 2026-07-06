# CLI & Integration Crates Review

**Scope:** `sley-cli`, `sley` (facade), and integration crates: `sley-config`, `sley-options`, `sley-refs`, `sley-rev`, `sley-ref-filter`, `sley-pathspec`, `sley-grep`, `sley-fsck`, `sley-formats`, `sley-notes`, `sley-pretty`, `sley-i18n`, `sley-procinfo`, `sley-strbuf-expand`, `sley-testkit`, `sley-bench`.  
**Date:** 2026-07-05

---

## Summary

`sley-cli` is **~286k lines** (~195k `src/`, ~74k `tests/`, 13.4k `lib.rs`) — larger than all engine crates combined and architecturally inverted relative to ADR 0001’s target (“thin shell over engines”). The integration crates are **well-factored tier-1/2 primitives**; the debt is almost entirely **tier-3 CLI-layer behavior trapped in the binary crate**.

**Headline findings:**

| Question | Answer |
|----------|--------|
| How much of `sley-cli` should move to library crates? | **~60–70% of non-dispatch code** over time: option parsing, repo setup, log/format engines, ancestry walks, patch orchestration, remote outcome formatting. Dispatch + argv/env side effects stay in CLI. |
| `GLOBAL_GIT_DIR` / global state | **8 process-wide `Mutex` globals** + **227 `discover_git_dir` call sites** — blocks embedders, complicates parallel tests, duplicates `setup.rs` / `RepositoryContext` / `sley::Repository`. |
| Hand-rolled parsing vs `sley-options` | **47 `parse_*_options`**, **79 `while let Some(arg)` loops** vs **4 modules** calling `sley_options::parse_options` (branch partial, diff_options, refs symbolic-ref, daemon). |
| Test distribution | **101 integration test files** (~74k lines), almost all **sley-vs-oracle-git parity** — correct for parity, **wrong shape for library extraction** (engines lack contract tests). |
| Extraction roadmap | ADR 0001 three-engine sequence: **A `sley-options` fan-out → B diff/rev setup → C format substrate**, then behavior waves into existing engines; `sley-cli` shrinks to dispatch + I/O. |

---

## Crate Inventory

| Crate | ~LOC | Role today | CLI coupling | Health |
|-------|------|------------|--------------|--------|
| **sley-cli** | 286k | Binary + god `lib` + 90 command modules | — | **Debt sink** — tier-3 behavior |
| **sley** | 7.1k (+1.1k tests) | Embedder facade (`Repository`, remote, notes, config edit) | **Zero** — CLI does not depend on it | Good API; underused by CLI |
| **sley-config** | 6.5k | Config parse/write/includes | Consumed everywhere | Solid; inline tests only |
| **sley-options** | 1.0k | `parse-options.c` pilot | 3–4 CLI consumers | Keystone; needs fan-out |
| **sley-refs** | 8.4k | Ref store/transaction | Low-level from CLI | Strong; 1 integration test |
| **sley-rev** | 12.4k | Revwalk, merge-base, graph | CLI bypasses graph paths in places | Engine good; CLI has duplicate BFS |
| **sley-ref-filter** | 1.5k | Ref `%` atoms via `strbuf-expand` | CLI re-exports + uses | Right layer; log engine parallel |
| **sley-pathspec** | 1.3k | `wildmatch.c` port | CLI + worktree | Clean single-file engine |
| **sley-grep** | 1.6k | Pattern/source model | CLI grep/log | Under-tested at crate boundary |
| **sley-fsck** | 4.5k | Object graph validation | `git fsck` command | Library-ready |
| **sley-formats** | 4.7k | Init, bundle, commit-graph | CLI + facade | Library-ready |
| **sley-notes** | 2.4k | Notes read/write | CLI + facade | Duplicate `ancestor_depths` in CLI |
| **sley-pretty** | 1.4k | Log format atoms | CLI log/show | Parallel to `lib.rs` log engine |
| **sley-strbuf-expand** | 1.3k | `%` expansion substrate | pretty + ref-filter | Substrate exists; dispatch not unified |
| **sley-i18n** | 232 | Optional gettext shims | Feature-gated | Fine as optional |
| **sley-procinfo** | 220 | Process ancestry for trace2 | CLI only | Correct CLI-adjacent placement |
| **sley-testkit** | 7.0k | Parity harness, hermetic git, fixtures | Dev-dep of CLI tests | Essential; not a product crate |
| **sley-bench** | 1.9k | Criterion for ODB/rev/refs | None | Good; gaps on diff/index/write |

---

## `sley-cli` Architecture

### Size distribution

```
lib.rs              13,437   (globals, discovery, shared helpers, log format engine)
remote_cmds.rs      13,140   (transport I/O shell atop sley-remote)
plumbing.rs         12,064   (porcelain + low-level commands)
branch.rs           10,974   (6+ option parsers; partial sley-options pilot)
pack.rs              8,083
rebase.rs            6,693
refs.rs              5,682
… + 83 more command modules
setup.rs               850   (setup_git_directory_gently port)
repository.rs          124   (RepositoryContext — 26 discover call sites)
```

**Total `commands/`:** ~195k lines across 90 modules. The four largest files alone are **49k lines** — not dispatch, but **trapped git behavior**.

### What `lib.rs` still owns (should not)

Despite verified waves into `commands/`, `lib.rs` remains a **13k-line shared heap**:

1. **Process globals** (`GLOBAL_GIT_DIR`, `GLOBAL_WORK_TREE`, `GLOBAL_BARE`, pathspec flags, lazy-fetch, replace-objects, config-parameter accumulator) — set in `run()` after `apply_global_options`.
2. **`discover_git_dir` / `discover_git_dir_by_walk`** (~line 12448) — 227 references across 60+ files; parallel to `setup::setup_git_directory` and `RepositoryContext::discover`.
3. **Log format engine** — `emit_compiled_log_format*`, `log_validate_*` (25+ validators in `lib.rs` alone, 67+ across CLI).
4. **`ancestor_depths` / `walk_commits`** — object-reading BFS duplicated vs `sley-rev` graph paths.
5. **Cross-command helpers** — status collection, config injection, refname warnings, pack helpers, attribute bridging.

`main.rs` is correctly thin (31 lines): `args_os` → `sley_cli::run` → exit code.

### Command module pattern

Each `commands/*.rs` file is typically:

- `cmd_*` entry (argv → options → repo open → work → print)
- Hand-rolled or partial `parse_*_options`
- Direct `discover_git_dir` + manual `FileObjectDatabase` / `FileRefStore` wiring
- `eprintln!` / `println!` for porcelain

**Partially migrated exemplars:**

- **`branch.rs`** — uses `sley_options::parse_options` for the main table, but still has **8 `parse_branch_*` mode-specific parsers** and mode-dispatch `if let` chains at the top of `cmd_branch`.
- **`diff_options.rs`** — strong `OptionSpec` table (~1.4k lines); shared across diff/log/show/stash family.
- **`refs.rs`** — symbolic-ref subcommand on `sley-options`.
- **`remote_cmds.rs`** — many paths already delegate to `sley_remote::fetch/push/clone` (179 `sley_remote::` refs) but file is still **13k lines** of URL resolve, progress, `FetchOutcome` formatting, and `cmd_*` wrappers.

### `sley-cli` does not use the `sley` facade

`Cargo.toml` wires **25 engine crates directly**; there is **no `sley` dependency**. The facade (`Repository`, `ConfigEdit`, `remote::RemoteContext`) is built for embedders (heddle/weft) but the CLI hand-assembles `FileObjectDatabase` + `FileRefStore` at hundreds of call sites. `RepositoryContext` (~26 uses) is a third, CLI-local handle — neither global `discover_git_dir` nor `sley::Repository`.

---

## Global State: `GLOBAL_GIT_DIR` and Friends

### Mechanism

```67:85:crates/sley-cli/src/lib.rs
static CMDLINE_CONFIG_PARAMETERS: Mutex<String> = Mutex::new(String::new());
static GLOBAL_GIT_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static GLOBAL_WORK_TREE: Mutex<Option<PathBuf>> = Mutex::new(None);
static GLOBAL_ATTR_SOURCE: Mutex<Option<String>> = Mutex::new(None);
static GLOBAL_BARE: Mutex<bool> = Mutex::new(false);
static GLOBAL_REPLACE_OBJECTS: Mutex<bool> = Mutex::new(true);
static GLOBAL_LAZY_FETCH: Mutex<bool> = Mutex::new(true);
// … LOCAL_REPO_ENV_HIDDEN, TRACE2_DEF_PARAMS_EMITTED, GLOBAL_PATHSPEC_FLAGS
```

`run()` copies parsed global flags into these mutexes before dispatch:

```170:176:crates/sley-cli/src/lib.rs
    set_global_git_dir(global.git_dir.clone());
    set_global_work_tree(global.work_tree);
    set_global_attr_source(global.attr_source);
    set_global_bare(global.bare);
    set_global_replace_objects(global.replace_objects);
    set_global_lazy_fetch(global.lazy_fetch);
    set_global_pathspec_flags(global.pathspec_flags);
```

`discover_git_dir` reads `explicit_git_dir()` → `global_git_dir().or_else(environment_git_dir)` and `global_bare()`, so **every command that calls `discover_git_dir` implicitly depends on prior global mutation in `run()`**.

`LOCAL_REPO_ENV_HIDDEN` + `with_local_repo_env_hidden` scoping exists so subcommands can temporarily ignore globals (e.g. `clone` into a fresh dir) — evidence the global model is already a footgun.

### Three discovery paths (drift risk)

| Path | Location | Env/globals? | Used by |
|------|----------|--------------|---------|
| **Global + walk** | `lib.rs::discover_git_dir` | Yes | ~227 call sites |
| **CLI setup trace** | `setup.rs::setup_git_directory` | Yes (faithful `setup.c`) | `GIT_TRACE_SETUP`, `rev-parse --show-*` |
| **Explicit context** | `repository.rs::RepositoryContext` | Via `discover_git_dir` | ~26 newer commands |
| **Library intrinsic** | `sley::Repository::discover` | **No** | Facade consumers only |

ADR 0001 and `setup.rs` document the intentional split: **`sley::Repository` is repo-intrinsic; env/cwd resolution belongs to a CLI layer**. The problem is that layer is **implicit global state** instead of an explicit `CliSession` / `RepositorySetup` passed through dispatch.

### Migration target

1. Introduce **`CliSession`** (or extend `RepositoryContext`) holding: resolved `(git_dir, common_dir, worktree, prefix)`, global flags, pathspec magic, config-parameter overlay.
2. Thread `&CliSession` through `cmd_*` (or a thin `CommandContext` trait) — **replace `discover_git_dir(&cwd)` reads**.
3. Keep **only** `run()` / alias dispatch mutating session state; delete `GLOBAL_*` mutexes.
4. Map `CliSession::resolve()` → `sley::Repository` for repo-intrinsic operations (config snapshot, remote, notes).
5. Leave `setup.rs` as the authoritative env resolver; **`discover_git_dir` becomes a private helper on session**, not a crate-root export used by 60 files.

**Effort:** Large (227 sites) but mechanical; can proceed in waves alongside `RepositoryContext` expansion.

---

## Option Parsing: Hand-Rolled vs `sley-options`

### Current split

| Approach | Scale | Location |
|----------|-------|----------|
| **`sley_options::parse_options`** | 4 call sites | `branch.rs`, `diff_options.rs`, `refs.rs` (symbolic-ref), `daemon.rs` |
| **`fn parse_*_options`** | **47 functions** | `submodule` (11), `worktree` (8), `pack` (5), `stash`, `am`, … |
| **`while let Some(arg)` loops** | **79 loops** | `plumbing` (14), `pack` (11), `remote_cmds` (7), … |
| **Mechanical helpers** | `args.rs` | `LongOption`, `GitArgCursor`, negation — **not** a full engine |

`sley-options` (~1k LOC) implements `OptionSpec`, typed values (`Bool`, `Int`, `Magnitude`, `Callback`), `OptFlags` (`NONEG`, `OPTARG`), usage generation, exit 129 — faithful to `parse-options.c` shape. It has **15 inline unit tests**; no integration tests.

### `args.rs` is explicitly not the solution

```1:7:crates/sley-cli/src/commands/args.rs
//! Small Git-shaped argument parsing helpers.
//!
//! Git's command line is not regular enough for a generic CLI derive layer:
//! commands disagree about option grouping, `--flag=value`, where parsing stops,
//! and the exact diagnostics/exit codes. These helpers keep the mechanical bits
//! shared while leaving command-specific compatibility decisions close to each
//! command parser.
```

ADR 0001 argues the opposite: disagreement is **declarative per-command** (`&[OptionSpec]`), not bespoke loops. `args.rs` should shrink to thin glue as tables absorb behavior.

### `log_validate_*` cluster

**67 `log_validate_*` references** across `lib.rs`, `log.rs`, `diff_options.rs`, `stash.rs` — per-option validators that `sley-options` typed values (`Magnitude`, color callbacks) should own once.

### Recommendation

- **Do not** grow `args.rs` or add clap/derive.
- **Do** migrate command families in dependency order: plumbing globals → diff family (extend `diff_options`) → transport (`remote_cmds`) → `submodule`/`worktree` (highest parser counts).
- Keep command-specific **post-parse validation** (mutual exclusion, mode dispatch) in `cmd_*`; move **syntax** (negation, `=value`, bundling, usage) to tables.

---

## `sley` Facade vs CLI

The facade (~2k LOC `Repository` + modules) is **the intended embedder surface**:

- Discovery without env (explicit path)
- `config_snapshot`, `remote`, notes, pack transfer, capabilities
- Re-exports under `plumbing::`

**CLI bypass is deliberate but incomplete:** `setup.rs` documents why `Repository::discover` ignores env. However, the CLI also bypasses facade for **operations the facade already wraps** (head, read_commit, ref updates, short status planning).

**Target state:**

| Layer | Owns |
|-------|------|
| `sley-cli` | `main`, `run`, global argv, `setup_git_directory`, trace2/procinfo, `cmd_*` dispatch, stdio |
| `sley` (+ engines) | All repo-mutating and repo-reading orchestration |
| `RepositoryContext` / `CliSession` | Bridge: env-resolved paths + lazy `sley::Repository` |

Add `sley` as a **dependency of `sley-cli`** once `CliSession` can construct `Repository` from resolved paths — avoids two parallel wiring graphs long-term.

---

## Integration Crates: Placement Assessment

### Correctly placed (keep)

- **`sley-config`**, **`sley-refs`**, **`sley-rev`**, **`sley-pathspec`**, **`sley-grep`**, **`sley-fsck`**, **`sley-formats`**, **`sley-notes`** — tier-1/2; CLI should only call, not host logic.
- **`sley-ref-filter`** + **`sley-pretty`** + **`sley-strbuf-expand`** — format atoms belong here; **commit log dispatch** does not (still in `lib.rs`).
- **`sley-procinfo`**, **`sley-i18n`** — legitimately CLI-adjacent.
- **`sley-testkit`**, **`sley-bench`** — dev/tooling; correct.

### Undersized / incomplete tier-3 (grow)

| Gap | Today | Target home |
|-----|-------|-------------|
| **parse-options** | `sley-options` pilot | Fan out; add `Color`, `DateMode` value types |
| **diff_options + setup_revisions** | `diff_options.rs` in CLI | New `sley-rev-setup` or submodule of `sley-rev` |
| **Command setup flags** | 150+ hand-rolled “not a git repository” | `NEED_WORK_TREE` declarative setup on session |
| **Log/graph format** | `lib.rs` ~2.5k-line engine | Extend `sley-pretty` + `sley-rev` walk metadata |
| **Repo setup (env)** | `setup.rs` in CLI | `sley-setup` crate or `sley::cli` module behind feature |
| **Patch porcelain** | CLI orchestrates `sley-diff-merge::render` | Move orchestration into `sley-diff-merge` or facade `diff` module |

---

## Test Distribution

### Numbers

| Location | Files | ~LOC | Style |
|----------|-------|------|-------|
| `sley-cli/tests/` | **101** | **~74k** | Subprocess parity vs oracle git |
| `sley-cli/src/**` `#[test]` | 22 modules | (embedded) | Unit tests in god files |
| `sley/tests/` | — | ~1.1k | Facade API tests |
| `sley-options` | inline | ~15 tests | Parser unit tests |
| `sley-config`, `sley-rev`, … | inline | varies | `#\[cfg(test)\]` modules |
| Other integration crates | 0–1 | minimal | Almost no `tests/` dirs |

### Pattern (typical integration test)

```50:56:crates/sley-cli/tests/log.rs
fn sley(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::sley_bin!(), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
}
```

Tests build temp repos, run **sley** and **oracle git** with identical argv, `assert_eq!` on status/stdout/stderr. `sley-testkit` provides hermetic env (`GIT_CONFIG_GLOBAL=/dev/null`), deterministic identity, `oracle_git()` version guard (2.55).

### Good or bad?

**Good for parity:** Byte fidelity is the product goal; end-to-end tests catch diagnostic drift that unit tests miss. Concentrating them in `sley-cli` matches where behavior currently lives.

**Bad for extraction:**

- Moving logic to `sley-rev` / `sley-options` **does not move tests** — regressions surface only through slow subprocess suites.
- **No contract tests** on `OptionSpec` tables per command (usage banner, exit 129 strings).
- Parallel / global-state tests are fragile (`GLOBAL_GIT_DIR` persists across tests in same process if any in-process CLI tests exist — most are subprocess-isolated, which masks the embedder bug).

### Recommendation

| Keep | Add |
|------|-----|
| `sley-cli/tests` parity files (oracle diff) | Crate-level tests for each extracted engine |
| `sley-testkit` harness | `sley-options` integration tests mirroring git’s `t/helper/test-parse-options` |
| Subprocess isolation | In-process `CliSession` tests without globals when session lands |
| | **Shrink** per-file integration tests as libraries gain coverage (faster CI) |

---

## What Should Move Out of `sley-cli` (Prioritized)

### Wave 0 — Quick wins (no API churn)

| Item | From | To | ~LOC | Risk |
|------|------|-----|------|------|
| `flatten_tree` wrappers | `lib.rs`, `read_tree.rs` | direct `sley_diff_merge` calls | tiny | Low |
| `workspace.rs` stub | CLI | delete | 1 | Low |
| `remote_cmds` pure formatters | CLI | `sley-remote` outcome types | 1–2k | Med |
| Duplicate `ancestor_depths` | `lib.rs`, notes, push | `sley_rev::ancestor_depths` | ~500 | Med |

### Wave 1 — `sley-options` fan-out (ADR Engine A)

Migrate parser families by count: `submodule`, `worktree`, `pack`, `stash`, `plumbing` globals, `remote_cmds`, remaining `branch` mode parsers. **Delete** corresponding `parse_*_options` and shrink `log_validate_*` into typed specs.

**Outcome:** ~30–40k lines of repetitive parsing → tables + shared validators.

### Wave 2 — Session + discovery (unblock embedders)

- `CliSession` replaces `GLOBAL_*`
- `setup.rs` → `sley-setup` (or `sley::setup` feature)
- `RepositoryContext` → `CliSession::repository()` → `sley::Repository`
- `discover_git_dir` crate-root export **removed**

**Outcome:** Library embedders and CLI share repo open path; globals gone.

### Wave 3 — `diff_options` + `setup_revisions` (ADR Engine B)

Extract `diff_options.rs` to library; add argv `(revs, pathspecs)` splitter used by `log`, `show`, `diff`, `rev-list`, `stash`, `format-patch`.

**Outcome:** Diff UI mutual-exclusion checked once; ambiguous rev/path diagnostic unified.

### Wave 4 — Format substrate (ADR Engine C)

- Move `emit_compiled_log_format*` from `lib.rs` into `sley-pretty` (or `sley-rev::log_format`)
- Unify with `sley-ref-filter` atom dispatch via `strbuf-expand`
- `DateMode` already in `sley-core` — wire through format engine

**Outcome:** One `%`-engine; `log`/`show`/`for-each-ref` share tables.

### Wave 5 — Trapped algorithms

| Behavior | From | To |
|----------|------|-----|
| Patch render orchestration | `format_patch.rs`, `lib.rs` | `sley-diff-merge::render` + facade |
| Merge-recursive virtual base | `merge_rebase/merge_util.rs` | `sley-diff-merge` |
| Rebase driver loop | `rebase.rs` | `sley-sequencer` orchestration API |
| Unpack-trees porcelain errors | CLI + `sley-unpack-trees` | `sley-unpack-trees` only |
| `.gitmodules` walks | `submodule.rs` | `sley-submodule::config` |

### Wave 6 — Thin `remote_cmds` / `plumbing`

After waves 1–5, these should be **&lt;3k lines each**: URL/display/stdio only, matching `fetch` template.

---

## Extraction Roadmap (Timeline)

```
Phase 0 (now)     │ sley-options pilots: branch, diff_options, symbolic-ref
                  │ sley-remote core extracted (done); remote_cmds wrappers remain
                  ▼
Phase 1 (1–2 mo)  │ sley-options → submodule, worktree, pack, stash
                  │ Contract tests per OptionSpec; CI gate on usage strings
                  ▼
Phase 2 (parallel)│ CliSession replaces GLOBAL_*; setup.rs extracted
                  │ sley-cli depends on sley facade for repo-intrinsic ops
                  ▼
Phase 3           │ diff_options + setup_revisions library crate
                  │ log_validate_* deleted; Magnitude/Color in sley-options
                  ▼
Phase 4           │ Log format engine → sley-pretty/sley-rev
                  │ ancestor_depths unified; merge-base all-graph
                  ▼
Phase 5           │ merge-rebase driver, format-patch orchestration → engines
                  │ remote_cmds/plumbing thin-shell pass
                  ▼
Target            │ sley-cli ~15–25k LOC (dispatch + setup + I/O)
                  │ Integration crates hold tier-3 tables + behavior
```

**Success metrics:**

- `sley-cli` LOC &lt; sum of engine crates (invert ADR ratio)
- `discover_git_dir` refs → 0 (session-only)
- `parse_*_options` → 0
- `sley-options` consumers → 40+ command modules
- Library crate `tests/` directories grow; `sley-cli/tests` LOC plateaus or shrinks

---

## Critical Issues

1. **Inverted crate ratio** — CLI ~286k vs engines ~85k; parity work adds handlers to CLI, not rows to tables.
2. **Triple repo wiring** — `discover_git_dir` globals, `RepositoryContext`, `sley::Repository` — embedders cannot safely interleave with CLI in-process.
3. **`sley-options` adoption stalled** — 4/90 command modules after pilot; 47 bespoke parsers remain.
4. **Log format engine in `lib.rs`** — blocks `sley-pretty`/`strbuf-expand` unification; highest-risk extraction.
5. **Tests don’t follow extraction** — 74k lines anchor behavior to CLI subprocess tests.

## Moderate Issues

- `branch.rs` pilot incomplete (8 mode parsers still hand-rolled).
- `remote_cmds.rs` 13k lines despite `sley-remote` extraction — documentation (`docs/sley-remote-extraction.md`) stale.
- `sley-cli` `#![allow(clippy::all, clippy::unwrap_used)]` — lint island (see `04-rust-practices.md`).
- Integration crates lack `tests/` dirs — discovery via CLI only.

## Positive Signals

- ADR 0001 is explicit and matches codebase evidence.
- `commands.rs` modular split is real (90 modules); not all still in one file.
- `setup.rs` is a clean, documented `setup.c` port (850 lines) — extractable as a unit.
- `diff_options.rs` proves `sley-options` scales to large shared tables.
- `sley-testkit` parity harness is production-quality (hermetic env, oracle version pin).
- `sley-remote` migration shows the thin-wrapper end state for transport.
- Facade (`sley`) is active with remote/notes/config-edit — ready for CLI adoption once session exists.

---

## Recommendations (Ordered)

1. **Declare extraction freeze on new hand-rolled parsers** — new options must be `OptionSpec` rows.
2. **Ship `CliSession` design doc + pilot** on one command family (`branch` already half-done).
3. **Add `sley-options` integration test crate** — git parse-options vectors before next fan-out wave.
4. **Extract `setup.rs` → `sley-setup`** — first standalone tier-3 crate; CLI re-exports.
5. **Add `sley` to `sley-cli` dependencies** — new code uses facade; old code migrates opportunistically.
6. **Log format extraction epic** — schedule after options wave 1 (touches show/stash/format-patch).
7. **Per-engine contract tests** when moving code out of CLI — do not rely solely on parity shrinkage.

---

*Cross-references: `docs/adr/0001-cli-layer-engines.md`, `reviews/2026-07-05/03-legacy-migration.md`, `reviews/2026-07-05/04-rust-practices.md`.*