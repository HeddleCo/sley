# Pre-Alpha Cleanup Plan

**Status:** In progress — `cleanup/pre-alpha` @ integration branch

### Progress log

| Wave | Status | Notes |
|------|--------|-------|
| W00 | ✅ done | expect_used, capability inlines, flatten_tree wrappers, README, docs |
| W10–W13 | ✅ done | pack cap, inflate bounds, URL redact, cred cap |
| W20 | ✅ done | sley-protocol module split |
| W21 | ✅ done | sley-pack module split |
| W22 | ✅ done | sley-diff-merge + absorb sley-diff-format |
| W23a–c | ✅ done | fanout prefix, mmap index paths |
| W23d–e | ✅ done | sley-odb module split (install/loose/pack/registry/reachability/repack) |
| W24 | ✅ done | lint burn-down non-CLI engines |
| W30 | ✅ done | unified ancestor_depths on sley-rev |
| W31 | ✅ done | log format → sley-pretty; CLI adapter wiring |
| W32 | ✅ done | superseded by W33 (0 `parse_*_options` remaining) |
| W33 | ✅ done | 0 `parse_*_options` remaining |
| W34 | ⚠️ partial | sley-rev diff_options module landed |
| W40 | ✅ done | engine parity harness |
| W41 | ✅ done | 203 engine parity tests (target 200) |
| W50 | ✅ done | CliSession + discovery.rs; 0 `discover_git_dir` / `GLOBAL_*` |
| W51 | ⚠️ partial | transport orchestration in `sley-remote`; filter parsing + `apply_configured_partial_clone_filter` migrated; bare/mirror/clone edge cases remain in CLI |
| W52 | ✅ done | 12 engine deps removed; `lib.rs` **322** LOC; `dispatch.rs` 490; 15+ helper modules |
| W52a | ✅ done | hooks + env-aware OpenOptions in sley facade |
| W60 | ✅ done | `branch/` tree; `dispatch.rs` **102** LOC; `positional.rs` legacy argv match |
| W61 | ✅ done | `plumbing.rs` → `plumbing/` tree (14 modules) |
| W62 | ⚠️ partial | `diff_render.rs` extracted (3.5k LOC) |
| W70 | ⚠️ partial | `sley-bench` suites exist; `baselines/` scaffold added |
| W72, W90 | ⏳ pending | CLI clippy burn-down, manual parity gate |
| W71 | ✅ done | sley-fetch → sley-remote::install |

**Decisions locked:** `sley-pretty` in place (no rename); hooks **(a)** → W52a; integration branch until W90; parity gate manual at end.  
**Date:** 2026-07-05  
**Branch baseline:** `main` @ `250e1f54`  
**Inputs:** Branch review (`reviews/2026-07-05/`), ADR 0001, GOAL.md, upstream-gap-map  
**Review target:** Claude Fable 5

---

## 1. Objectives

### Primary

1. **Shrink duplicate Rust implementations** — delete superseded code paths; keep a single canonical implementation per behavior.
2. **Make `sley` the correctness engine** — library APIs drive behavior; CLI becomes a thin, testable shell.
3. **Preserve git on-disk/protocol interop** — parsers and wire codecs for real `.git` repos and real remotes stay; our *parallel Rust copies* of the same logic go.
4. **Hold the upstream parity floors** on the enrolled `t/*.sh` subset (see §3): zero per-script regressions below floor at the final gate, with incidental gains banked as new floors. Validated **once at the end** of this cleanup (intermediate breakage allowed). *Note: floors are per-script assertion counts, not 100% pass — closing floors → 100% is feature work (transport frontier, revwalk options) and belongs to the post-cleanup parity roadmap per ADR 0001's compute-completion epics, not this cleanup.*

### Secondary

5. **Build a faster parity harness** — library-level oracle tests replace CLI subprocess for most development feedback.
6. **Eliminate god files** — no `lib.rs` > ~3k lines in any crate post-cleanup.
7. **Harden untrusted-input paths** — fetch pack caps, bounded inflate, credential redaction (security review H1/M1–M4).

### Non-goals

- Rewriting git format specs or dropping legacy **on-disk** git artifacts (packed-refs, old remote files, protocol v0/v1, etc.).
- Achieving parity on excluded command families (§3.2).
- Feature development unrelated to structural debt (new porcelain commands, new flags).

---

## 2. Architecture Target (v1)

### Layering

```
┌─────────────────────────────────────────────────────────────┐
│  sley-cli          dispatch, env, hooks, human I/O only     │
│  (~25–40k LOC target)                                       │
└──────────────────────────┬──────────────────────────────────┘
                             │ uses
┌──────────────────────────▼──────────────────────────────────┐
│  sley              Repository, OpenOptions, remote, notes     │
│  (embedder + harness entry)                                   │
└──────────────────────────┬──────────────────────────────────┘
                             │ composes
┌──────────────────────────▼──────────────────────────────────┐
│  Tier 3 engines    sley-options, sley-pretty* (expanded),     │
│                    diff/rev options engine (W34, ADR B)       │
└──────────────────────────┬──────────────────────────────────┘
                             │
┌──────────────────────────▼──────────────────────────────────┐
│  Tier 2 engines    rev, diff-merge†, worktree, sequencer,     │
│                    remote‡, protocol, transport, pathspec, …  │
└──────────────────────────┬──────────────────────────────────┘
                             │
┌──────────────────────────▼──────────────────────────────────┐
│  Tier 1 storage    core, object, odb, pack, index, refs,    │
│                    mmap, formats, config                      │
└─────────────────────────────────────────────────────────────┘

† absorbs sley-diff-format
‡ absorbs sley-fetch
* sley-pretty expanded **in place** with log/ref atom dispatch (from CLI lib.rs).
  NOT renamed to `sley-format` — one letter from the existing `sley-formats`
  (on-disk formats: reftables/commit-graph/bundles) is a permanent footgun.
```

### Context model (replaces globals)

```rust
// crates/sley-cli/src/session.rs (new)
pub struct CliSession {
    pub repo: sley::Repository,
    pub cwd: PathBuf,
    pub env: CliEnv,           // inherited GIT_* , trace, pager flags
    pub setup: SetupFlags,     // RUN_SETUP / NEED_WORK_TREE equivalent
}
```

- **Delete:** `GLOBAL_GIT_DIR`, `GLOBAL_WORK_TREE`, `GLOBAL_BARE`, `discover_git_dir` (~225 sites).
- **Delete:** parallel `RepositoryContext` where it duplicates `sley::Repository` discovery.
- CLI `run()` builds one `CliSession` per invocation; commands receive `&CliSession`.

### Crate consolidation (merge once, no future split)

| Action | Rationale | Won't split later because |
|--------|-----------|---------------------------|
| `sley-diff-format` → `sley-diff-merge` | 707 LOC, zero tests, only consumed by diff/render path | Formatting is a diff output concern; git's `diff.c` + `xdiff` are one subsystem |
| `sley-fetch` → `sley-remote` | 608 LOC install glue, no independent API surface | Git has no separate "fetch crate"; install is receive-pack/upload-pack completion |
| Expand **`sley-pretty` in place** (no rename) | Unify log + ref `%` dispatch | git's `pretty.c` + ref-filter share `strbuf_expand`; splitting again recreates ADR debt. Rename to `sley-format` rejected: collides with existing `sley-formats` |
| **Do not merge** `sley-protocol` ↔ `sley-transport` | Wire codec vs HTTP/SSH client are distinct test/security boundaries | Protocol fuzzing and transport TLS are separate lifecycles |
| **Do not merge** `sley-options` into CLI | Tier-3 engine per ADR 0001 | Options tables must be shared by CLI and future harness |

### Size targets (post-cleanup)

| Artifact | Today | Target |
|----------|-------|--------|
| `sley-cli` total | ~286k LOC | **≤ 40k** |
| `sley-cli/src/lib.rs` | 13,437 | **≤ 500** (dispatch + session only) |
| `remote_cmds.rs` | 13,140 | **deleted** (logic in `sley-remote`) |
| Hand-rolled `parse_*_options` | 47 | **0** |
| God files > 10k lines | 8 | **0** |
| `sley-cli` direct tier-1/2 engine deps | 25+ | **0** (facade-only, see W52) |
| Workspace crates | 35 | **33** (−2 merges) |

---

## 3. Scope & Exclusions

### 3.1 In scope — CLI parity

All standard git plumbing/porcelain commands **except** §3.2, matching the enrolled upstream subset in `.github/workflows/upstream-parity.yml` (~891 `t[0-9]*.sh` scripts, floor-gated).

Oracle version: **git 2.55.0** (align `GOAL.md` / `PARITY.md` text with `sley_core::UPSTREAM_GIT_COMPAT_VERSION`).

### 3.2 Out of parity gate (may remain stubbed/minimal)

Per `crates/sley-testkit/upstream-gap-map.txt` and product direction:

| Family | Commands / scripts | Plan |
|--------|-------------------|------|
| **Mailbox / email** | `send-email`, `mailinfo`, `mailsplit`, `imap-send`, `am`, `format-patch` | Not in upstream floor; keep stubs if needed for help/alias surface; **no cleanup work** unless blocking other waves |
| **Legacy VCS** | git-svn, git-cvs, `cvsserver`, `p4`, gitweb | Explicitly out of scope; stub `unknown command` acceptable |
| **Optional i18n shell** | `git-compat-i18n` feature | Delete feature + `sley-i18n` shell shims unless needed for gettext tests |

### 3.3 Keep — git legacy interop (not our debt)

These are **upstream git compatibility**, not "our old impls":

- `augment_with_legacy_remote_files` (read `remotes/` / `branches/` dirs)
- Deprecated `[remote.origin]` config header forms
- Protocol v0 / v1 alongside v2
- `crlf` ↔ `text` attribute alias
- Index v2/v3/v4 writers, pack v2, OFS/ref deltas

### 3.4 Delete — our superseded Rust paths

| Debt | Action |
|------|--------|
| `*_with_rename_options` API family | Collapse into `DiffNameStatusOptions` |
| Slow `ancestor_depths` (CLI, remote, notes) | Single `sley_rev` graph-first implementation |
| `emit_compiled_log_format*` in CLI `lib.rs` | Move to `sley-pretty` (expanded) |
| `remote_cmds.rs` orchestration | Move to `sley-remote`; CLI prints outcomes |
| `flatten_tree` wrappers, `workspace.rs` stub | Delete |
| Stale capability flip constants | Inline / delete indirection |
| `RepositoryContext` duplicate discovery | `sley::Repository` only |
| Hand-rolled option parsers (47) | `sley-options` tables |

---

## 4. Harness Strategy

### Problem

Today: ~891 upstream scripts × subprocess `sley` × full CLI stack ≈ slow feedback.  
Goal: **`sley::Repository` + engines** invoked directly in tests; oracle remains upstream `git` binary.

### Tracks

| Track | When | Deliverable |
|-------|------|-------------|
| **H1 — Infrastructure** | Phase 2 (parallel) | `sley-testkit::engine_parity` module: hermetic repo fixture, run engine fn, diff stdout/stderr/exit/files against oracle |
| **H2 — Port high-value scripts** | Phase 3–5 | Port top 50 failing t-file *patterns* to library tests (init, cat-file, rev-parse, update-index, diff, log) |
| **H3 — CLI gate (final)** | Phase 8 | Full `run-upstream-tests-waves.sh` on enrolled subset; floors must not regress |

### Harness acceptance (Phase 8)

```bash
# Final gate (from repo root)
SLEY_UPSTREAM_WAVES=8 SLEY_TEST_TIMEOUT=240 \
  crates/sley-testkit/scripts/run-upstream-tests-waves.sh

# Floor check
.github/workflows/scripts/check-parity-floors.sh
```

Intermediate phases: `cargo build --workspace` required; `cargo test -p <crate>` **green for each wave's owned crates** (workspace-wide `cargo test` best-effort; cross-wave breakage allowed). From Phase 2 onward, run the upstream waves **report-only** on the integration branch weekly to bound regression debt early — the floor *gate* still applies only at Phase 8.

### Library test growth target

By end of Phase 5: **≥ 200 new engine-level parity tests** covering the 10 highest-traffic command families (init, config, cat-file, rev-parse, update-index, ls-files, commit, log, diff, checkout). These become the fast loop during Phase 6–7 CLI surgery.

---

## 5. Execution Model

### Branching

- **Integration branch:** `cleanup/pre-alpha` (long-lived; force-push allowed pre-alpha).
- **Wave branches:** `cleanup/wXX-short-name` from integration; merge back when wave acceptance criteria met.
- **Parallelism:** disjoint path ownership (§6); use git worktrees per wave.

### Build policy

| Phase | `cargo build` | `cargo test` | Upstream parity |
|-------|---------------|--------------|-----------------|
| 0–1 | required | owned crates green; workspace best-effort | not gated |
| 2–7 | required | owned crates green; workspace best-effort | **report-only weekly** (no gate) |
| 8 | required | workspace required | **gated** |

### Review cadence

Each wave PR includes: scope paths, deleted APIs list, harness tests added, known breakage list.

---

## 6. Wave Catalog

Waves are **disjoint by path ownership**. Waves in the same **Phase** marked `(∥)` can run in parallel.

---

### Phase 0 — Foundation

#### W00 — Workspace policy & dead weight

| Field | Value |
|-------|-------|
| **Owns** | `Cargo.toml`, `crates/*/Cargo.toml` (lint attrs only), `crates/sley-cli/src/commands/workspace.rs`, `README.md`, `docs/sley-remote-extraction.md`, `crates/sley-remote/src/capabilities.rs`, wrapper fns in `sley-cli/src/lib.rs` + `read_tree.rs` |
| **Depends** | — |
| **Parallel** | — |

**Tasks**

- [ ] Add `expect_used = "deny"` to `[workspace.lints.clippy]`
- [ ] Delete `workspace.rs` stub + `mod workspace`
- [ ] Delete `flatten_tree` thin wrappers; call `sley_diff_merge::flatten_tree` at sites
- [ ] Remove stale `HTTP_PROTOCOL_V2_FETCH` etc. indirection in `capabilities.rs`
- [ ] Fix README `git-cli` → `sley-cli`; reconcile 2.54 vs 2.55 in `GOAL.md` / `PARITY.md`. *Caution: the floors in `check-parity-floors.sh` were recorded against git 2.54.0 while the workflow now pins 2.55.0 — if reconciling triggers a floor re-baseline under 2.55, record any count deltas explicitly*
- [ ] Feature-gate the `sley-testkit` production dependency in `sley-cli` (used by `commands/utility.rs`) — test-harness code out of the default binary
- [ ] Archive `docs/sley-remote-extraction.md` → `docs/plans/completed-remote-extraction.md`

**Acceptance**

- `cargo build --workspace` succeeds
- `rg 'flatten_tree' crates/sley-cli` shows no local wrapper fns
- `rg 'GLOBAL_GIT_DIR' | wc -l` unchanged (globals removed in W50)

---

### Phase 1 — Security hardening `(∥)`

#### W10 — Fetch pack size cap

| **Owns** | `crates/sley-odb/src/lib.rs` (install path), `crates/sley-fetch/src/lib.rs`, `crates/sley-remote/src/fetch.rs` |
| **Depends** | W00 |

- [ ] Add `fetch.maxInputSize` / `transfer.maxSize` config keys (mirror git)
- [ ] Enforce in `install_raw_pack_from_reader_with_options`; fail closed
- [ ] Unit test: truncated stream errors before unbounded disk write

#### W11 — Bounded inflate (shared helper)

| **Owns** | `crates/sley-pack/src/lib.rs` (export), `crates/sley-diff-merge/src/lib.rs`, `crates/sley-cli/src/commands/plumbing.rs` |
| **Depends** | W00 |

- [ ] Extract `bounded_inflate_reserve` to `sley_pack::inflate` (public)
- [ ] Wire `git_patch_delta`, `inflate_zlib_exact`
- [ ] Fix `parse_leading_usize` overflow → error, not `usize::MAX` saturate
- [ ] Regression tests for allocation bombs (port from `sley-pack` tests)

#### W12 — URL credential redaction

| **Owns** | `crates/sley-core/src/lib.rs` (`redact_unsafe_urls`), `crates/sley-remote/src/fetch.rs`, `crates/sley-remote/src/push.rs`, `crates/sley-cli/src/commands/remote_cmds.rs` (until W51 deletes) |
| **Depends** | W00 |

- [ ] Add `redact_url_for_display(url) -> String` in `sley-core`
- [ ] Apply to FETCH_HEAD, prune output, push error messages
- [ ] Tests: `https://user:pass@host/repo.git` → redacted

#### W13 — Credential helper read cap

| **Owns** | `crates/sley-transport/src/lib.rs` |
| **Depends** | W00 |

- [ ] Cap `read_git_credential` at 64 KiB; clear error on overflow

---

### Phase 2 — Engine crate surgery `(∥)`

*No `sley-cli` edits except W11/W12 touch plumbing/remote_cmds.*

#### W20 — `sley-protocol` decomposition

| **Owns** | `crates/sley-protocol/**` |
| **Depends** | W00 |
| **LOC** | 14,224 → modules ≤ 3k each |

**Target modules:** `pktline`, `sideband`, `v0`, `v1`, `v2`, `upload_pack`, `receive_pack`, `tests/`

- [ ] Mechanical `mod` split; `pub use` preserves all paths
- [ ] `cargo test -p sley-protocol` passes
- [ ] No behavior change (refactor-only)

#### W21 — `sley-pack` decomposition

| **Owns** | `crates/sley-pack/**` |
| **Depends** | W00, W11 (inflate module name) |

**Target modules:** `index`, `delta`, `read`, `write`, `stream`, `inflate`

#### W22 — `sley-diff-merge` decomposition + merge `sley-diff-format`

| **Owns** | `crates/sley-diff-merge/**`, delete `crates/sley-diff-format/**`, update dependents |
| **Depends** | W00, W11 |

**Tasks**

- [ ] Move `sley-diff-format/src/*` → `sley-diff-merge/src/format/`
- [ ] Update `sley`, `sley-cli`, `Cargo.toml` workspace (remove `sley-diff-format` member)
- [ ] Split `lib.rs` → `line_diff`, `blob_merge`, `name_status`, `patch`, `merge_trees`, `render`, `format/`
- [ ] **API collapse:** delete `*_with_rename_options`; extend `DiffNameStatusOptions` with `detect_inexact`, limits
- [ ] Add **≥ 30 unit tests** for format/hunk/word-diff (was 0)
- [ ] Rename-detection blob cache: `Arc<[u8]>` keyed by OID

**Acceptance**

- `rg 'with_rename_options' crates/` → 0
- `rg 'sley-diff-format' | wc -l` → 0 (except changelog)
- `sley-diff-merge` no file > 4k lines

#### W23 — `sley-odb` performance + decomposition (sequential sub-steps)

| **Owns** | `crates/sley-odb/**`, `crates/sley-index/**` (mmap read only) |
| **Depends** | W00, W10 |
| **Assignee** | **Single owner** — all touch `sley-odb/src/lib.rs` |

| Sub-step | Work |
|----------|------|
| W23a | Fanout-aware `resolve_prefix` / `object_ids_with_prefix` (no full enumeration) + add the `resolve_prefix` 1k/100k bench **here** (moved from W70 to break the acceptance cycle) |
| W23b | Route `cached_pack_index` through `load_pack_index_data`; use reverse index for offset→oid |
| W23c | `sley-index::read_repository_index` uses `sley-mmap::open_index` |
| W23d | Split `lib.rs` → `loose`, `pack`, `registry`, `repack`, `reachability`, `install` |
| W23e | Reduce mutex layering: document invariants; `parking_lot::RwLock` for read-heavy caches |

**Acceptance**

- Bench: `resolve_prefix` on 100k-object fixture < 10ms (bench lands in W23a; W70 extends the suite)
- No `object_ids()` call from `resolve_prefix` code path

#### W24 — Lint burn-down (non-CLI engines)

| **Owns** | `sley-core`, `sley-config`, `sley-rev`, `sley-object`, `sley-index`, `sley-refs`, `sley-worktree`, `sley-archive`, `sley-mmap` |
| **Depends** | W00 |

- [ ] `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]` per crate
- [ ] Fix all production `expect`/`unwrap` in these crates
- [ ] `cargo clippy --workspace` clean for owned crates (CLI excluded)

---

### Phase 3 — Library seam unification `(∥)`

#### W30 — Ancestry unification

| **Owns** | `crates/sley-rev/**`, `crates/sley-remote/src/push.rs`, `crates/sley-notes/**`, CLI call sites for `ancestor_depths` / `walk_commits` |
| **Depends** | W24 |

- [ ] Promote the **existing** `sley_rev::ancestor_depths_with_graph` (`sley-rev/src/lib.rs:6486`) to the single public graph-first, shallow-graft-aware entry point (migration onto an existing seam, not greenfield)
- [ ] Delete CLI `lib.rs::ancestor_depths`, remote copy, notes copy
- [ ] Migrate `merge_rebase/merge_base.rs`, `plumbing.rs`, `format_patch.rs`, `replay.rs` callers

#### W31 — Format engine extraction

| **Owns** | `crates/sley-pretty/**` (expanded **in place** — no rename; `sley-format` collides with `sley-formats`), `crates/sley-ref-filter/**`, `crates/sley-strbuf-expand/**`, CLI `lib.rs` log format fns |
| **Depends** | W24 |

- [ ] Expand `sley-pretty` in place with atom dispatch for log + ref formats
- [ ] Move `emit_compiled_log_format*` into the format engine; `log_validate_*` **value validators** go to `sley-options` typed values (per review 08), format-atom validation stays here
- [ ] Delete ~2.5k lines from CLI `lib.rs`
- [ ] `sley-ref-filter` and log formatting share atom table / `ExpandFormat` substrate

#### W32 — `sley-options` wave A

| **Owns** | `crates/sley-options/**`, CLI files: `submodule.rs`, `worktree.rs`, `pack.rs`, `stash.rs`, `am.rs` |
| **Depends** | W00 |

- [ ] Migrate 5 highest-parser-count commands to `OptionSpec` tables
- [ ] Delete local `parse_*_options` in those files
- [ ] Parity: usage text + exit 129 behavior matches prior (document intentional deltas)

#### W33 — `sley-options` wave B

| **Owns** | Remaining CLI `commands/*.rs` with `parse_*_options` |
| **Depends** | W32 |

- [ ] Migrate remaining ~42 parsers
- [ ] Delete `args.rs` mechanical duplicates
- [ ] `rg 'fn parse_.*_options' crates/sley-cli` → 0

#### W34 — Diff/rev options engine (ADR 0001 Engine B)

| **Owns** | new `setup` module in `crates/sley-rev/**` (or `sley-rev/src/setup.rs`), `crates/sley-cli/src/commands/diff_options.rs`, `setup_revisions`-equivalent call sites in `diff.rs`, `log.rs`, `show`, `stash.rs`, `format_patch.rs` |
| **Depends** | W30, W33 |

ADR 0001 names three keystone tier-3 engines; W32/W33 build A (parse-options) and W31 builds C (format substrate). This wave builds **B**, previously missing from the plan.

- [ ] Extract shared diff-UI options struct (git's `struct diff_options`): output-format bitmask, `--name-only`/`--name-status`/`-s`/`--check` mutual exclusion — declared once, consumed by diff/log/show/stash/format-patch instead of ~20 flags re-declared each
- [ ] Extract `setup_revisions` argv → `(revs, pathspecs)` splitter: owns the `--` boundary and the "ambiguous argument: unknown revision or path" diagnostic (re-implemented in 5+ commands today)
- [ ] CLI `diff_options.rs` shrinks to table declarations over the engine

**Acceptance:** one implementation of the rev-vs-path argv split; the "ambiguous argument" diagnostic emitted from a single shared site

---

### Phase 4 — Harness infrastructure `(∥ Phase 3 tail)`

#### W40 — Engine parity testkit

| **Owns** | `crates/sley-testkit/**`, new `crates/sley/tests/parity/**` or `sley-testkit/src/engine_parity.rs` |
| **Depends** | W24 (stable `Repository` API) |

- [ ] `EngineParityCase { name, setup, run_sley, run_oracle, compare }`
- [ ] Helpers: `hermetic_repo()`, `assert_bytes_eq`, `assert_stdout_eq`
- [ ] Port 10 reference tests from existing CLI integration tests (cat-file, rev-parse, config)
- [ ] Document harness env in `crates/sley-testkit/README` (new)

#### W41 — Engine parity expansion

| **Owns** | `crates/sley/tests/parity/**`, per-family modules |
| **Depends** | W40, W30, W31 |

- [ ] 200 library parity tests across 10 command families (see §4)
- [ ] CI job `engine-parity` (fast, PR gate during Phase 6–8)

---

### Phase 5 — CLI spine (sequential)

#### W50 — `CliSession` replaces globals

| **Owns** | `crates/sley-cli/src/session.rs` (new), `setup.rs`, `lib.rs` globals, **all** `discover_git_dir` call sites |
| **Depends** | W30, W33, **W40** (+ first ~50 W41 tests recommended) — enforces risk R2's mitigation: the engine harness must be live before globals are torn out |

- [ ] Introduce `CliSession`; thread through `run()` → command dispatch
- [ ] Delete `GLOBAL_*` statics
- [ ] Replace `discover_git_dir()` with `session.repo` / `Repository::discover`
- [ ] Delete `repository.rs` `RepositoryContext` if fully superseded

**Acceptance:** `rg 'GLOBAL_GIT_DIR|discover_git_dir' crates/sley-cli` → 0

#### W51 — Delete `remote_cmds.rs`

| **Owns** | `crates/sley-remote/**` (grow), delete `crates/sley-cli/src/commands/remote_cmds.rs`, thin `remote.rs` command module |
| **Depends** | W50, W12 |

- [ ] Move fetch/push/clone/ls-remote orchestration + `FetchOutcome` types to `sley-remote`
- [ ] CLI remote commands: parse args → call `sley::remote` → format output
- [ ] Delete 13k-line file

#### W52 — CLI depends on `sley` facade

| **Owns** | `crates/sley-cli/Cargo.toml`, `lib.rs` dispatch, command signatures |
| **Depends** | W50, W51 |

- [ ] Add `sley` dependency to `sley-cli`
- [ ] Commands use `sley::Repository`, `sley::GitError`, re-exports
- [ ] **Remove direct tier-1/2 engine deps from `sley-cli/Cargo.toml` as commands migrate.** End state: `sley-cli` depends only on `sley` + `sley-options` (+ `sley-procinfo`, optional `sley-i18n`, `sley-testkit` behind the W00 feature gate). This makes the CLI the first embedder — every facade gap becomes a compile error instead of being papered over by a direct engine import; it is the forcing function for the embeddability goal
- [ ] Gut `lib.rs` to ≤ 500 lines: dispatch table, session, global option application, hook runner

**Acceptance:** `rg '^sley-' crates/sley-cli/Cargo.toml` lists only the facade set above (stragglers may be swept in W90, but each must be listed in the W52 PR description)

---

### Phase 6 — CLI leaf splits `(∥ after W52)`

#### W60 — `branch.rs` decomposition

| **Owns** | `crates/sley-cli/src/commands/branch/**` |
| **Depends** | W52, W33 |

- [ ] Split 11k file into submodules: `create`, `delete`, `move`, `list`, `edit`, etc.
- [ ] Business logic that isn't I/O → `sley-refs` / `sley-rev` helpers where possible

#### W61 — `plumbing.rs` decomposition

| **Owns** | `crates/sley-cli/src/commands/plumbing/**` |
| **Depends** | W52 |

- [ ] Split by command family; each handler ≤ 300 lines

#### W62 — Diff/merge command thinning

| **Owns** | `diff.rs`, `merge.rs`, `merge_rebase/**`, `read_tree.rs`, `checkout.rs` |
| **Depends** | W22, W34, W52 |

- [ ] Remove orchestration duplicated in engines; CLI = options + format + exit code
- [ ] `virtual_ancestor_entry_map` lives in `sley-diff-merge` only (from W22 if not done)

---

### Phase 7 — Transport, perf, benches `(∥)`

#### W70 — Library benchmarks

| **Owns** | `crates/sley-bench/**` |
| **Depends** | W23 |

- [ ] Benches: prefix 1k/100k, pack write, diff rename matrix, mmap on/off, concurrent `read_object`
- [ ] Publish baseline CSV in `crates/sley-bench/baselines/`

#### W71 — Transport parity gaps

| **Owns** | `crates/sley-transport/**`, `crates/sley-remote/**`, merge `sley-fetch` here |
| **Depends** | W51 |

- [ ] Merge `sley-fetch` into `sley-remote` (`remote::install` module)
- [ ] Wire `filter`, `deepen-since`/`deepen-not` over HTTP; SSH v2 or hard error with clear message
- [ ] Fresh `UreqHttpClient` → shared agent per operation batch (connection reuse)

#### W72 — `sley-cli` lint burn-down

| **Owns** | `crates/sley-cli/**` |
| **Depends** | W52, W60–W62 |

- [ ] Remove `#![allow(clippy::all, clippy::unwrap_used)]` from `lib.rs`
- [ ] Fix all production `expect`/`unwrap`
- [ ] Add CLI to CI blocking clippy set

---

### Phase 8 — Final gate

#### W90 — Parity restoration & verification

| **Owns** | Entire repo (fix regressions only) |
| **Depends** | All prior waves |

- [ ] `cargo test --workspace` green
- [ ] `cargo clippy --workspace -- -D warnings` green
- [ ] Full upstream wave run on enrolled 891 scripts
- [ ] `check-parity-floors.sh` — no regressions below floor; bank any gains as new floors
- [ ] `sley-cli/Cargo.toml`: no tier-1/2 engine dependencies remain (sweep W52 stragglers)
- [ ] Delete `git-compat-i18n` feature if unused
- [ ] Security-documentation pass from §12 deferred list (L1–L3 shell-execution threat model, L6 TLS feature matrix) folded into the docs update
- [ ] Update `TRACKER.md`, `GIT_PARITY_CHECKLIST.md` with cleanup completion notes

**Success criteria:** §1 primary objectives met; floors pass (zero regressions, gains banked); engine parity tests ≥ 200; metrics in §2 size targets within 20% (CLI LOC may land 40–60k during parity fix — tighten in follow-up).

---

## 7. Dependency Graph

```
W00
 ├─(∥) W10 W11 W12 W13
 ├─(∥) W20 W21 W24
 │      W22 (after W11)
 │      W23 (after W10, sequential internal)
 ├─ W30 (after W24)
 ├─ W31 (after W24)
 ├─(∥) W32 → W33 → W34 (W34 also after W30)
 ├─ W40 (after W24) → W41
 │
 W50 (after W30, W33, W40)
  → W51 (after W50, W12)
  → W52 (after W51)
  →(∥) W60 W61 W62 (W62 also after W22, W34)
  → W72
 W70 (after W23)
 W71 (after W51)
 W90 (after all)
```

**Maximum parallel tracks:** 7 (W10–13 + W20–21 + W24 + W32 after W00; W40 joins once W24 lands).

---

## 8. Risk Register

| ID | Risk | Likelihood | Impact | Mitigation |
|----|------|------------|--------|------------|
| R1 | W23 ODB perf refactor introduces object read regressions | Med | High | W23a–c each land with unit tests; W70 bench before/after |
| R2 | W50 global removal breaks obscure `GIT_DIR` edge cases | High | Med | **Enforced in the graph:** W50 depends on W40 (engine harness live); port CLI integration tests before W50; keep parity script list |
| R3 | W31 format extraction changes log output bytes | Med | High | Byte-level engine parity tests before/after; defer to Phase 8 bulk fix |
| R4 | W22 rename API collapse breaks diff callers | Low | Med | Single PR with mechanical `rg` verification |
| R5 | Crate merges (`diff-format`, `fetch`) churn dependents | Low | Low | One commit per merge; `pub use` re-exports at old paths in `sley::plumbing` for one release cycle **optional** — pre-alpha: break immediately |
| R6 | Phase 8 parity fix takes longer than structural work | High | Med | Weekly **report-only** upstream runs from Phase 2 (§5), not just 6–7; fix top 20 regressions first |
| R7 | God-file splits cause merge conflicts between parallel waves | Med | Low | Strict path ownership; rebase integration branch daily |

---

## 9. Metrics & Checkpoints

| Checkpoint | After | Metric |
|------------|-------|--------|
| CP1 | Phase 1 | 4 security issues closed; `expect_used` denied |
| CP2 | Phase 2 | 0 files > 10k LOC; 2 crates merged |
| CP3 | Phase 3 | 0 hand-rolled parsers; 0 `ancestor_depths` duplicates; ADR Engine B extracted (W34) |
| CP4 | Phase 4 | 200 engine parity tests; harness docs |
| CP5 | Phase 5 | 0 globals; `remote_cmds.rs` deleted; `lib.rs` < 1k LOC |
| CP6 | Phase 8 | Upstream floors pass; clippy clean |

**Weekly dashboard** (script to add in W40):

```bash
# scripts/cleanup-metrics.sh
wc -l crates/sley-cli/src/lib.rs crates/sley-cli/src/commands/remote_cmds.rs 2>/dev/null
rg -c 'fn parse_.*_options' crates/sley-cli
rg -c 'discover_git_dir|GLOBAL_GIT_DIR' crates/sley-cli
find crates -name lib.rs -exec wc -l {} + | sort -n | tail -5
```

---

## 10. Remaining Questions

These do not block Phase 0–1; answers needed before Phase 5:

1. **Integration branch landing:** Keep all waves on `cleanup/pre-alpha` until W90, or merge phases to `main` incrementally?
   - *Recommendation:* merge phases 0–2 to `main` early (low CLI touch); keep Phase 5–7 on integration branch.

2. **`am` / `format-patch` implementation depth:** Leave as-is (out of parity gate), or actively stub to minimal `unknown`/`not supported` to shrink CLI?
   - *Recommendation:* leave as-is unless they block W32 parser migration.

3. **`sley-format` naming:** ~~New crate `sley-format` vs expand `sley-pretty` in place?~~ **Resolved (Fable review):** expand `sley-pretty` in place, no rename — `sley-format` is one letter from the existing `sley-formats` (on-disk formats crate) and would be a permanent footgun for embedders reading the crate list.

4. **Engine parity CI:** Make `engine-parity` a required PR check from Phase 4 onward?
   - *Recommendation:* yes, once W41 lands 50 stable tests.

5. **Hooks & env-honoring discovery — library or CLI?** **Resolved:** **(a)** — promote hook-runner and env-aware `OpenOptions` into `sley` (W52a, before facade-only deps). Embedders and harness get git-correct hook execution without subprocess CLI.

---

## 11. Fable Review Checklist

- [ ] Every **Critical/High/Medium** review finding from `reviews/2026-07-05/` maps to ≥1 wave task; remaining items are explicitly listed in §12 Deferred Findings (nothing silently dropped)
- [ ] Path ownership is disjoint across parallel waves
- [ ] Git legacy interop (§3.3) is not conflated with Rust duplicate deletion (§3.4)
- [ ] CLI parity goal (floor-hold per §1.4, not 100%) scoped to enrolled upstream subset, not mail/VCS (§3.2)
- [ ] Harness strategy enables library-first correctness without abandoning CLI oracle gate
- [ ] Crate merges justified against future split (§2)
- [ ] Sequential spine (W50→W51→W52) correctly blocks CLI surgery on unfinished options/ancestry
- [ ] W23 single-owner constraint documented
- [ ] Phase 8 acceptance is measurable (floors script, not vibes)
- [ ] Risks R1/R3/R6 have concrete mitigations

### Finding → Wave mapping (complete)

| Review finding | Wave |
|----------------|------|
| H1 fetch pack cap | W10 |
| M1/M2 inflate bounds | W11 |
| M3 URL redaction | W12 |
| M4 credential cap | W13 |
| O(N) prefix resolution | W23a |
| mmap bypass | W23b–c |
| Mutex layering | W23e |
| diff-merge blob clone | W22 |
| `expect_used` gap | W00, W24, W72 |
| sley-cli clippy allow | W72 |
| God files (8) | W20–W23 (protocol, pack, diff-merge, odb), W31/W52 (cli lib.rs), W51 (remote_cmds), W60–W61 (branch, plumbing) |
| 47 parsers | W32–W33 |
| ADR Engine B missing (`diff_options`/`setup_revisions` CLI-resident, review 08 CLI-7) | W34 |
| GLOBAL_GIT_DIR | W50 |
| remote_cmds 13k | W51 |
| ancestor_depths 3× | W30 |
| log format in lib.rs | W31 |
| rename dual API | W22 |
| diff-format 0 tests | W22 |
| sley-fetch thin crate | W71 |
| Bench gaps | W70 |
| Transport HTTP/SSH gaps | W71 |
| Stale docs/stubs | W00 |
| `sley-testkit` prod dep in CLI (found in Fable review) | W00 |
| Hooks/env placement for embedders | §10 Q5 (decide before W52) |

---

## 12. Deferred Findings (explicitly out of this cleanup)

Tracked so the §11 coverage claim is honest; none block the v1 architecture. Revisit in the post-cleanup parity/perf roadmap unless noted.

| Finding (review) | Why deferred |
|---|---|
| P-6/P-7/P-8 pack-write perf: `DeltaIndex` reuse, deferred up-front hashing, streaming write (02 §6–8) | Perf feature work; W21 splits the file and W70 establishes the bench baseline first |
| P-9 corruption-fallback reparse, P-11 `packed-refs` handle cache, P-12 loose-read allocation (02 §9, 11, 12) | Perf; bench-first |
| P-13–P-16, P-18 micro-allocation items (02) | Low impact |
| DW-3 index↔worktree diff-path unification (`IndexWorktreeContext`, 07 §1.2) | Behavior-risk refactor; wants the W41 engine-test safety net in place first |
| DW-7 `sley-worktree` residual god modules (index/checkout/status/ignore/filter, 2.6–4.2k each) + DW-9 test relocation | Below the 10k threshold; wave-47 pattern exists as the template; schedule post-W90 |
| DW-8/DW-13 worktree↔diff-merge decoupling, `read_tree` probe/writer adapters → `sley-worktree` | Bundle with DW-7 |
| DW-10 unpack-trees sparse-dir TODO arms, DW-11 porcelain error catalog centralization | Parity feature work (spike: ~70–90 t-cells on divergent restore/reset paths) |
| DW-12 / legacy §7: 13 `.gitmodules` walks → `sley-submodule::config` | W32 migrates submodule *option parsers* only; schedule with post-cleanup submodule parity |
| CS-1 `sley-core` scope creep (`DateMode`, trace2 extraction) | W31 may pull `DateMode` naturally; rest post-cleanup |
| CS-2 `ByteString`/`BString` dedup, CS-3 `GitError::Exit(i32)`, CS-9 `sley-mmap::open` visibility | API hygiene, post-alpha |
| CS-4/CS-5 archive: test expansion, streaming `.tar.gz`, generic `ObjectReader` in `_full` APIs | Off the parity-critical path |
| CS-6 index edge-case tests, CS-8 index/archive/core benches | Add to W70 backlog if time permits |
| NT-2 `git://` v2 auto-negotiation, NT-4 transport/protocol framing dedup, NT-5 wildcard `use sley_protocol::*`, NT-7 mocked-HTTP/SSH-v2 tests | Post-cleanup transport epic (W71 follow-ups) |
| RP-4 `Result<_, String>` error channels, RP-5 `#[allow]` audit, RP-6 `ORIGINAL_CWD` → `OnceLock`, RP-7 `missing_docs` policy, RP-9 ref-name newtypes, `sley-procinfo` 0 tests (RP-8) | Hygiene backlog, post-W90 |
| L1–L3 inherited shell-execution threat-model docs, L4 protocol `read_to_end` note, L5 symlink-checkout note, L6 TLS feature matrix | Documentation-only → folded into the W90 docs pass |

*(Pulled back in-plan during review: CS-7 `expect_used` on `sley-archive`/`sley-mmap` → W24; the `resolve_prefix` bench → W23a; `sley-testkit` feature gate → W00.)*

---

## 13. Suggested Start

**Immediate (this week):** W00 + W10–W13 in parallel (4 agents/worktrees).

**Next:** W20 + W21 + W24 in parallel; W23 owner starts W23a after W10 merges.

**Do not start:** W50 until W33 **and W40** complete (options tables + a live engine harness must exist before session threading; see R2).

---

*End of plan.*