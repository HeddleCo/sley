# Rust Practices Review

Audit date: 2026-07-05. Sampled across ~20 crates; focus on lint policy, error design, module size, concurrency primitives, and test distribution.

## Summary

The workspace enforces `unsafe_code=forbid`, `unwrap_used=deny`, `dbg_macro=deny`, and `todo=deny` at the workspace level (`Cargo.toml:85–91`), but enforcement is **uneven in practice**. Several security-sensitive crates (`sley-pack`, `sley-protocol`, `sley-odb`, `sley-refs`, `sley-formats`, `sley-transport`, `sley-fetch`) add a stronger crate attribute `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]` — a good pattern that most other crates lack.

The largest gap is **`sley-cli`**, which blanket-allows all Clippy lints and `unwrap_used` (`crates/sley-cli/src/lib.rs:1–8`), plus widespread production `.expect()` that is invisible to the workspace lint because **`expect_used` is not denied** workspace-wide. CI only blocks Clippy on `sley-pack` and `sley-protocol` (`.github/workflows/ci.yml:94–108`); the rest of the workspace (~6,241 violations per CI comment) is report-only.

Architecturally, the codebase is a faithful git port: correctness-oriented, with strong newtypes in `sley-refs` and a central `GitError` in `sley-core`. The main Rust-practice debt is **monolithic `lib.rs` files**, **inconsistent lint rigor**, and **test concentration in `sley-cli`** rather than library crates.

## Critical Issues

### 1. `sley-cli` disables all Clippy and `unwrap_used`

```1:8:crates/sley-cli/src/lib.rs
#![allow(
    dead_code,
    unused_assignments,
    unused_mut,
    unused_variables,
    clippy::all,
    clippy::unwrap_used
)]
```

This single attribute neutralizes the workspace `unwrap_used = "deny"` policy for the largest crate (~13k-line `lib.rs`, 10k-line `branch.rs`, 12k-line `plumbing.rs`). Production code uses `.expect()` freely, e.g.:

- `crates/sley-cli/src/commands/replay.rs:333,349,365` — mode dispatch after parse (should be `if let Some` / `ok_or`)
- `crates/sley-cli/src/commands/format_patch.rs:835` — `commits.last().expect(...)` in cover-letter path
- `crates/sley-cli/src/commands/format_patch.rs:3993` — `write_fmt` to `Vec` via `expect`
- `crates/sley-cli/src/commands/rev_parse.rs:80` — `pseudo.as_mut().expect("just inserted")`

**Risk:** Panics in user-facing CLI paths; lint burn-down cannot progress while the root module suppresses everything.

### 2. Workspace denies `unwrap_used` but not `expect_used`

`Cargo.toml:88–91` sets `unwrap_used = "deny"` only. Production `.expect()` is widespread and **not linted** in crates without the stronger crate attribute:

| Location | Line | Context |
|----------|------|---------|
| `sley-diff-merge/src/lib.rs` | 9980 | `group.into_iter().next().expect("non-empty")` after `group.len() > 1` branch — logic invariant, but should use `if let Some` |
| `sley-core/src/lib.rs` | 841, 1919 | `ObjectId::to_hex()`, `to_hex()` — `fmt::Write` to `String` |
| `sley-config/src/lib.rs` | 1631, 1778, 2167, 2171, 2279, 2362 | Parser/peek invariants |
| `sley-rev/src/revlist.rs` | 189, 318, 375 | `ready` queue non-empty loop invariant |
| `sley-rev/src/setup.rs` | 268+ | `strip_prefix(...).expect("prefix matched")` in `value if` arms |
| `sley-rev/src/lib.rs` | 2569, 2630 | Lazy-init cache `expect` after `is_none()` check |
| `sley-cli/src/commands/hash_object_fsck.rs` | 215, 788 | Documented size preconditions |

Crates with `#![cfg_attr(not(test), deny(..., clippy::expect_used))]` avoid this; others do not.

### 3. God files impede review, testing, and reuse

| File | Lines (approx.) |
|------|-----------------|
| `sley-protocol/src/lib.rs` | 14,224 |
| `sley-cli/src/lib.rs` | 13,437 |
| `sley-diff-merge/src/lib.rs` | 13,405 |
| `sley-cli/src/commands/plumbing.rs` | 12,064 |
| `sley-cli/src/commands/branch.rs` | 10,974 |
| `sley-odb/src/lib.rs` | 10,492 |
| `sley-pack/src/lib.rs` | 10,319 |
| `sley-refs/src/lib.rs` | 8,198 |

These modules mix parsing, I/O, algorithms, and CLI glue. Refactoring into submodules (as `sley-diff-merge` partially does with `ws`, `render`, `range`) would improve compile times and allow targeted lint/test gates.

## Moderate Issues

### Error type design

**Central `GitError` is reasonable but string-heavy** (`sley-core/src/lib.rs:1703–1717`):

```1703:1717:crates/sley-core/src/lib.rs
pub enum GitError {
    Io(String),
    InvalidObjectId(String),
    InvalidObject(String),
    InvalidFormat(String),
    InvalidPath(String),
    Unsupported(String),
    NotFound(NotFoundKind),
    Transaction(String),
    Command(String),
    Cli(CliExit, String),
    Exit(i32),
}
```

- `NotFound(NotFoundKind)` and `Cli(CliExit, String)` are well-typed; most other variants embed free-form `String`, losing structure for programmatic handling.
- `impl From<std::io::Error> for GitError` exists (`sley-core/src/lib.rs:1810`) — good.
- **Ad-hoc `String` errors** outside `GitError`:
  - `sley-submodule/src/update_strategy.rs:45` — `Result<UpdateStrategy, String>`
  - `sley-options/src/lib.rs:24` — callback `Result<Option<String>, String>`
  - `sley-cli/src/commands/diff_options.rs:953`, `pack.rs:7439`, `lib.rs:8460` — `Result<_, String>`
  - `sley-rev/src/lib.rs:2100` — `commits: Option<Result<HashMap<...>, String>>` caches parse failures as raw strings

`Result<T>` returning owned `String` values (e.g. `remote_names()`, `encode_refspec()`) is idiomatic for git-compatible output; the concern is **`String` as the error channel**, not as success payload.

### `#[allow(...)]` suppressions

| Pattern | Count / scope | Justified? |
|---------|---------------|------------|
| `clippy::too_many_arguments` | 80+ across CLI, diff-merge, notes, worktree | **Mostly yes** — git C API arity; consider `struct` parameter bags long-term |
| `clippy::unwrap_used` (test modules) | `sley-config/src/lib.rs:2750`, `raw_edit.rs:1208,1423` | **Yes** — scoped to `#[cfg(test)]` |
| `clippy::too_many_lines` | `replay.rs:1373` | **Marginal** — signals file should split |
| `dead_code` | `sley-pretty/src/lib.rs:189,204,471,508`; `sley-refs/src/lib.rs:3726`; `sley-cli/.../add_interactive.rs:1042` | **Weak** — placeholder/WIP code; prefer `#[cfg(...)]` or delete |
| `clippy::all` + `unwrap_used` | `sley-cli/src/lib.rs:1–8` | **No** — defeats workspace policy |

### Arc/Mutex usage

**`sley-odb`** — layered caches (`crates/sley-odb/src/lib.rs:4712–5245`):

```4712:4712:crates/sley-odb/src/lib.rs
type PackBytesCache = Arc<Mutex<HashMap<PathBuf, Arc<PackData>>>>;
```

Nested `Arc<Mutex<Arc<Mutex<...>>>>` for pack/delta/index caches is performance-motivated for shared `FileObjectDatabase`, but hard to reason about. Worth documenting invariants and considering `parking_lot::Mutex` or shard maps if contention shows up.

**`sley-core`** — global `Mutex<Option<PathBuf>>` for `original_cwd` (`sley-core/src/lib.rs:11–20`). A `OnceLock` or thread-local would be simpler for process-lifetime state; current code silently drops errors on `lock()` failure.

**`sley-pack`** — 39 `Arc`/`Mutex`/`RwLock` references; **`sley-cli/pack_objects.rs`** — 16. Justified for parallel pack indexing, but should stay out of hot parsing paths.

### Public API surface and documentation

- **`sley`** facade (`crates/sley/src/lib.rs:1–41`) has excellent crate-level docs and a clear `Repository` API.
- **`sley-core`** exports many public items without crate-level `#![deny(missing_docs)]` or per-item docs (e.g. `DateMode`, trace hooks).
- **`sley-strbuf-expand`** exports trait-based expansion with module docs (`lib.rs:1–5`) but public types like `ExpandAtom<A>` lack field docs (`lib.rs:26–29`).
- **Re-export surface**: `sley::plumbing` re-exports entire engine crates — convenient but broad; consumers can depend on internals unintentionally.

### Proc-macro crate (`sley-strbuf-expand`)

Not a proc-macro — it is a **generic trait-driven parser/expander** (`AtomTable`, `AtomResolver`, `ExpandFormat<A>`). This is a **positive** design: no `syn`/`quote` dependency, testable without macro expansion, shared between `sley-pretty`, `sley-ref-filter`, etc. No issues found with macro hygiene (N/A).

### Test quality and distribution

| Area | Finding |
|------|---------|
| **Integration tests** | ~100 files under `crates/sley-cli/tests/` — primary fidelity gate vs upstream git |
| **Unit tests in libraries** | Present in most crates via `mod tests` in `lib.rs` (e.g. `sley-protocol` 153, `sley-pack` 89, `sley-refs` 59) |
| **Thin / missing** | `sley-diff-format` — **0** `#[test]`; `sley-procinfo` — **0**; `sley-bench` — **0** (bench-only) |
| **`sley-testkit`** | 6.9k-line integration harness — good for CLI, but library crates should not rely solely on CLI tests |
| **Cross-crate** | `sley/tests/` (5 files), `sley-refs/tests/cas.rs`, `sley-index/tests/intent_to_add.rs` — sparse |

CLI module tests (`sley-cli/src/commands/*.rs`) duplicate coverage with `tests/*.rs` in places — acceptable for fast feedback but increases maintenance.

### Naming and newtypes

**Strong:** `sley-refs` ref name newtypes (`BranchRefName`, `BranchRefNameBuf`, `TagRefName`, `FullRefNameBuf` at `lib.rs:4780+`) with `From` conversions and validation at construction.

**Weaker:** `Ref.name: String`, `RefTarget::Symbolic(String)` (`sley-refs/src/lib.rs:29–32`) — could use `FullRefNameBuf` consistently. `ObjectId` is a solid newtype in `sley-core`.

### Missing crate-level `#![deny(...)]`

No crate uses `#![deny(missing_docs)]`. Only a subset use the stronger unwrap/expect deny:

- **Have** `cfg_attr(not(test), deny(unwrap_used, expect_used))`: `sley-formats`, `sley-odb`, `sley-refs`, `sley-protocol`, `sley-transport`, `sley-pack`, `sley-fetch`
- **Lack** (rely on workspace `unwrap_used` only): `sley-diff-merge`, `sley-config`, `sley-rev`, `sley-cli`, `sley-core`, `sley-worktree`, etc.

## Minor / Style

- **`clone_on_copy = "allow"`** workspace-wide (`Cargo.toml:92`) — fine for git-style structs; watch for accidental clones on hot paths.
- **Edition 2024 / rust-version 1.96** — consistently declared; good.
- **`sley-config` `split_key`** uses `expect` on canonical keys (`lib.rs:2167,2171`) where `debug_assert!` + `unwrap_or` fallback would document the invariant.
- **`sley-pretty` `FormatTier` / `FormatFields`** marked `#[allow(dead_code)]` while sibling methods are public — incomplete feature or dead scaffolding.
- **Inconsistent error propagation in lazy caches** (`sley-rev/src/lib.rs:2624`) — `.map_err(|err| err.to_string())` erases error type before re-wrapping as `GitError::InvalidFormat`.
- **`sley-cli` `hash_object_fsck.rs:215`** — `expect` with comment explaining NUL precondition; acceptable as documented invariant if `expect_used` is denied and reviewed.

## Positive Patterns

1. **Workspace lint inheritance** via `[lints] workspace = true` in crate `Cargo.toml` files — consistent wiring.
2. **Security-parser hardening** — `sley-odb`, `sley-protocol`, `sley-pack` crate-level `deny(expect_used)` with comments referencing sley#7.
3. **`GitError` helpers** — `object_not_found`, `remote_not_found`, `usage`, `user_error` (`sley-core/src/lib.rs:1742–1774`).
4. **`sley-strbuf-expand` trait design** — composable, no proc-macro magic; clear separation of scan vs. atom resolution.
5. **Ref name newtypes** in `sley-refs` — validation at boundary, `Deref` to `str`, `From` chains.
6. **`sley` facade documentation** — embedder-oriented module docs with `no_run` example.
7. **Test `expect` messages** — many tests use `.expect("test operation should succeed")` for grep-friendly failures (acceptable in tests).
8. **`RefDeleteError`** — separate error enum with `From<std::io::Error>` (`sley-refs/src/lib.rs:54–89`), not everything folded into `GitError`.

## Per-Crate Notes (brief, grouped)

**Foundation:** `sley-core` — solid types; global `Mutex` cwd; production `expect` in `to_hex`/`ObjectId::to_hex`. `sley-object`, `sley-index` — inline tests OK; no extra crate denies.

**Config / formats:** `sley-config` — parser `expect`s in production; test-only `allow(unwrap_used)`. `sley-formats` — strong crate deny; good model.

**Storage / transport:** `sley-odb`, `sley-pack` — large monoliths, strong lint, heavy `Arc<Mutex>` caching. `sley-protocol`, `sley-transport`, `sley-fetch` — strong lint; protocol file oversized.

**Diff / merge:** `sley-diff-merge` — 13k `lib.rs`, submodule split started; **missing** `expect_used` deny; one production `expect` at `lib.rs:9980`. `sley-diff-format` — **no tests**.

**Refs / rev:** `sley-refs` — newtypes + strong lint; one `dead_code` allow. `sley-rev` — 9k `lib.rs`; many production `expect`s in setup/revlist/graph paths; `String` error cache.

**Worktree / pathspec / grep:** `sley-worktree` — 75 unit tests in `lib.rs`; `too_many_arguments` allows. `sley-pathspec` — 18 tests. `sley-grep` — 2 tests only.

**Remote / submodule:** `sley-remote` — modular `src/` layout (better than monolith); not in blocking CI clippy set. `sley-submodule` — `Result<_, String>` for strategy errors.

**CLI / facade:** `sley-cli` — worst lint suppression; tests are comprehensive but crate is unmaintainable size. `sley` — good public API docs; thin integration tests.

**Support:** `sley-strbuf-expand` — clean trait API. `sley-options` — `String` error callbacks mirror git getopt. `sley-pretty` — `dead_code` allows on tier machinery. `sley-testkit` — 6.9k-line git runner (test infra, not prod). `sley-procinfo`, `sley-mmap`, `sley-bench` — minimal or no unit tests.

## Recommended Actions

### P0 — Policy alignment

1. Add `expect_used = "deny"` to `[workspace.lints.clippy]` in `Cargo.toml` (alongside `unwrap_used`).
2. Remove `clippy::all` and `clippy::unwrap_used` from `sley-cli/src/lib.rs:1–8`; adopt incremental `allow` only where needed during burn-down.
3. Extend `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]` to `sley-diff-merge`, `sley-config`, `sley-rev`, `sley-core`, `sley-worktree` (priority order by user-input exposure).

### P1 — Eliminate production panics

4. Replace high-traffic `expect`s with explicit branches:
   - `sley-diff-merge/src/lib.rs:9980` → `if let Some(m) = group.into_iter().next()`
   - `sley-core` `to_hex` / `ObjectId::to_hex` → `write_hex` returning `String` via infallible helper or `unwrap_unchecked` + SAFETY comment (if truly proven)
   - `sley-cli` replay/format_patch/rev_parse paths → `ok_or_else(|| GitError::...)`
5. Fix `sley-rev` `strip_prefix().expect("prefix matched")` — bind prefix in `if let` or use `&value[prefix.len()..]`.

### P2 — Structure and errors

6. Split god files: start with `sley-cli/commands/branch.rs`, `plumbing.rs`, then `sley-protocol`, `sley-diff-merge`, `sley-odb` into domain modules (parse / plan / execute).
7. Migrate `Result<_, String>` error paths (`sley-submodule`, `sley-options` callbacks) to `GitError` or small crate-local enums with `Display`.
8. Replace `sley-rev` `Result<HashMap, String>` cache with `Result<HashMap, GitError>` or a `CommitGraphError` type.

### P3 — Quality gates

9. Add unit tests to `sley-diff-format` and `sley-procinfo`.
10. Graduate crates into CI blocking Clippy set as they clean up (per `.github/workflows/ci.yml` burn-down plan).
11. Audit `#[allow(dead_code)]` in `sley-pretty`, `sley-refs`, `sley-cli` — remove or gate with features.
12. Consider `#![warn(missing_docs)]` on `sley` and `sley-core` public API first, then expand.

### P4 — Concurrency review

13. Document thread-safety invariants for `FileObjectDatabase` caches; evaluate `parking_lot` or `RwLock` for read-heavy pack index cache.
14. Replace `sley-core` `ORIGINAL_CWD` `Mutex` with `std::sync::OnceLock<Option<PathBuf>>` if single-writer semantics suffice.