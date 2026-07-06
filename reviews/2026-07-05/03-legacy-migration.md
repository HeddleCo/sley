# Legacy Code & Migration Review

**Scope:** `/Users/lukethorne/dev/HeddleCo/sley` — dual paths, compatibility shims, migration flags, dead modules, and git-parity legacy aliases.  
**Date:** 2026-07-05

---

## Summary

sley’s largest legacy surface is not old Git file formats (those are required for parity) but **parallel CLI-layer implementations** accumulated during gap-closing: ~40k lines in `sley-cli` (`lib.rs` 13.4k, `branch.rs` 11k, `remote_cmds.rs` 13.1k) still carry behavior that ADR 0001 says belongs in tier-3 engines (`sley-options`, shared diff/rev setup, unified format substrate).

**Already migrated (safe to treat as done):**
- `workspace.rs` checkout hand-roll → stub (1 line); routing via `read_tree.rs::checkout_two_way_engine` + `sley-unpack-trees::twoway_merge`
- Remote orchestration core → `sley-remote` (CLI wrappers remain in `remote_cmds.rs`)
- `sley-options` pilot on `branch.rs`, `diff_options.rs`, `refs.rs` (symbolic-ref)
- Patch rendering largely in `sley-diff-merge::render` (CLI orchestrates options/colors)

**Highest-value deletion/migration targets:**
1. **47 hand-rolled `parse_*_options` functions** vs 3 files on `sley-options` — largest structural debt (ADR 0001)
2. **`GLOBAL_GIT_DIR` / `discover_git_dir` global state** (~225 call sites) — blocks embedder/library use
3. **Triplicate `ancestor_depths` walks** (CLI, `sley-remote`, `sley-notes`) vs graph-accelerated `sley-rev` — performance + correctness drift
4. **Dual log/ref format engines** (`lib.rs` `emit_compiled_log_format*` ~2.5k lines vs `sley-pretty` / `sley-ref-filter` / `sley-strbuf-expand`)
5. **Stale migration docs** (`docs/sley-remote-extraction.md`, capability flip comments, README `git-cli` alias)

**Do not delete (Git compatibility shims):** legacy `remotes/`/`branches/` files, deprecated `[section.subsection]` config headers, `crlf`↔`text` attribute alias, protocol v0/v1 alongside v2, index v2 writer paths, `RenameDetectionOptions` default exact-only behavior.

---

## Deletable / Migratable Paths

| Item | Where | Why legacy | Migration target | Deletion risk | Effort |
|------|-------|------------|------------------|---------------|--------|
| **`workspace.rs` module stub** | `crates/sley-cli/src/commands/workspace.rs` (1 line) | Checkout logic moved to `read_tree.rs` / `checkout.rs`; file is an empty stub | Remove module + `mod workspace` if any | **Low** — no code | **S** |
| **README `git-cli` package alias** | `README.md` (~200 lines of `cargo run -p git-cli`) | Crate is `sley-cli`; no `git-cli` in workspace `Cargo.toml` | Replace with `-p sley-cli` or document alias if intended | **Low** — docs only | **S** |
| **Stale capability flip constants** | `crates/sley-remote/src/capabilities.rs` (`HTTP_PROTOCOL_V2_FETCH`, `SSH_CLONE_SUPPORTED`, `BUNDLE_FETCH_SUPPORTED` all `true` with “Flip once…” comments) | Migration scaffolding; behavior is live | Delete const indirection; inline `true` or derive from tests; update comments | **Low** | **S** |
| **`flatten_tree` thin wrappers** | `sley-cli/src/lib.rs:1999`, `read_tree.rs:467` | Duplicate one-line delegates to `sley_diff_merge::flatten_tree` | Call `sley_diff_merge::flatten_tree` directly at call sites | **Low** | **S** |
| **`docs/sley-remote-extraction.md`** | Entire doc | Describes pre-extraction state (“shallow NOT implemented”, “HTTP v2 unsupported”, logic in `lib.rs`); reality is `sley-remote` + wrappers in `remote_cmds.rs` | Archive or rewrite as “completed migration” note | **Low** if archived | **S** |
| **`remote_cmds.rs` transport wrappers** | `crates/sley-cli/src/commands/remote_cmds.rs` (~13.1k lines; 179 `sley_remote::` refs) | Thin URL/config/print layers atop library (`fetch_http_repository` → `sley_remote::fetch`) | Collapse into shared CLI outcome formatters; keep only I/O | **Med** — large file, many tests | **M** |
| **`GLOBAL_GIT_DIR` / `GLOBAL_BARE` globals** | `sley-cli/src/lib.rs:68–71`, ~225 `discover_git_dir` refs | Thread-local-ish mutable repo state from early CLI | `RepositoryContext` / `sley::Repository` passed explicitly; per ADR 0001 setup flags | **High** — pervasive | **L** |
| **Hand-rolled option parsers (47)** | `sley-cli` commands (`submodule.rs` 11, `worktree.rs` 8, `pack.rs` 5, …) | Pre-`sley-options` gap-closing | `sley-options` `OptionSpec` tables + shared validators | **Med** — byte parity on usage/errors | **L** |
| **`log_validate_*` cluster (14+ fns)** | `lib.rs`, `diff_options.rs`, `log.rs`, `stash.rs` | Per-option validators duplicated across commands | `sley-options` typed values (`Magnitude`, `Color`, etc.) | **Med** — exact error strings | **M** |
| **Log format engine in `lib.rs`** | `emit_compiled_log_format*`, `format_log_format_decorations` (~lines 10577–11886) | ADR: format trapped in CLI; `log_format.rs` absorbed into `lib.rs`/`commands/log/` | Unified substrate over `sley-strbuf-expand` + `sley-pretty` atoms | **High** — log/show/stash/format-patch | **L** |
| **`virtual_ancestor_entry_map`** | `merge_rebase/merge_util.rs:460` | Merge-recursive strategy stranded in CLI per ADR | `sley-diff-merge` (with `merge_trees`) | **Med** — merge correctness | **M** |
| **Submodule `.gitmodules` walks (13 sites)** | `submodule.rs` — 1 pilot migrated, 13 TODO | Hand-rolled section walks | `sley-submodule::config` helpers | **Med** | **M** |
| **Slow `ancestor_depths` copies** | `lib.rs:2009`, `remote_cmds.rs` (via import), `sley-remote/push.rs:2380`, `sley-notes:868` | Object-reading BFS; graph path exists in `sley-rev` | Single `sley_rev::ancestor_depths` (graph-first, shallow-aware) | **Med** — shallow graft semantics differ slightly in CLI copy | **M** |
| **`merge_rebase/merge_base.rs` depth walks** | Still calls CLI `ancestor_depths` heavily | TRACKER #58 fixed merge-base fast path for common cases; multi-commit criss-cross still slow | Route all merge-base logic through `sley_rev::merge_bases` / graph depths | **Med** | **M** |
| **`RepositoryContext` vs `sley::Repository`** | `sley-cli/repository.rs` (~24 commands) vs `crates/sley` facade | Dual discovery/object wiring; CLI deliberately keeps env/cwd in setup layer | Adopt facade for repo-intrinsic ops; keep CLI-only setup separate per `setup.rs` | **Med** | **M** |
| **`RenameDetectionOptions` parallel API** | `sley-diff-merge` `*_with_options` vs `*_with_rename_options` | Back-compat struct so old struct literals compile | Fold `detect_inexact` into `DiffNameStatusOptions` with `#[non_exhaustive]` migration | **Med** — many callers | **M** |
| **Unpack-trees porcelain error catalog** | Duplicated strings in CLI vs `sley-unpack-trees` `reject_merge` | Spike doc: partial/divergent vs git `setup_unpack_trees_porcelain` | Centralize in `sley-unpack-trees` | **High** — exact message parity | **L** |
| **Legacy remote file synthesis** | `sley-config/remotes.rs::augment_with_legacy_remote_files` | Pre-config-file `remotes/` + `branches/` dirs | **Keep** for parity; optional future: config flag to skip synthesis in greenfield tools | **High** if removed | **N/A** (compat) |
| **Deprecated config `[remote.origin]` form** | `sley-config/src/lib.rs`, `raw_edit.rs` | Git deprecated dotted headers | **Keep** until upstream drops | **High** | **N/A** |
| **`git-compat-i18n` shell shims** | `sley-i18n` — materialized `git-sh-i18n` scripts | gettext fallthrough for git shell scripts | Keep behind optional feature; not default path for Rust CLI | **Low** if feature-gated | **S** (doc only) |

---

## Dual Implementation Hotspots

### 1. CLI option parsing (biggest hotspot)

| Path | Location | Notes |
|------|----------|-------|
| **New** | `sley-options` — `branch.rs`, `diff_options.rs`, `refs.rs` (symbolic-ref) | ~3 of ~50 command modules |
| **Legacy** | 47 `fn parse_*_options` across `submodule`, `worktree`, `pack`, `stash`, `am`, … | Each re-implements `--no-`, `=value`, bundling |

**Why legacy:** ADR 0001 gap-closing before `sley-options` existed.  
**Risk:** Usage banners, exit 129, negation, and “takes no value” errors drift per command.

### 2. Commit ancestry / revwalk

| Path | Location | Notes |
|------|----------|-------|
| **Fast (graph)** | `sley_rev::walk_commit_metadata*`, `merge_bases`, `ancestor_depths_with_graph` | Used by optimized `rev-list`/`log`/`merge-base` paths (TRACKER #58–#61) |
| **Slow (object BFS)** | `ancestor_depths` in `lib.rs`, `push.rs`, `notes.rs`; `walk_commits` in `plumbing.rs`, `format_patch`, `replay`, `merge_rebase` | Still default for many commands |

**Migration target:** One graph-aware ancestry API in `sley-rev`; CLI/library call it uniformly.

### 3. Format / pretty-printing

| Path | Location | Notes |
|------|----------|-------|
| **Shared substrate** | `sley-strbuf-expand::ExpandFormat` | Used by `sley-pretty`, `sley-ref-filter` |
| **Commit log** | `lib.rs` `emit_compiled_log_format*` + `commands/log.rs` | Large bespoke state machine (notes, graph, diff injection) |
| **Ref listing** | `sley-ref-filter` + `sley-pretty::CompiledFormat` | Parallel atom tables |

ADR 0001 cites two independent `%`-engines; `strbuf-expand` partially unifies parsing, not atom dispatch.

### 4. Repository discovery

| Path | Location | Notes |
|------|----------|-------|
| **Global** | `GLOBAL_GIT_DIR`, `discover_git_dir` in `lib.rs` | ~225 references |
| **CLI local** | `RepositoryContext::discover` in `repository.rs` | ~24 commands |
| **Library** | `sley::Repository::discover` | Active in-crate; CLI intentionally bypasses for env/cwd (`setup.rs`) |

### 5. Transport / fetch

| Path | Location | Notes |
|------|----------|-------|
| **Library** | `sley-remote::{fetch, push, clone, ls_remote, fetch_bundle}` | Orchestration + capabilities |
| **CLI shell** | `remote_cmds.rs` — URL resolve, `eprintln!`, `FetchOutcome` formatting, `cmd_*` | 13k lines; many `fetch_*_repository` wrappers already delegate |

Extraction doc’s “move from lib.rs” is **done**; remaining debt is CLI formatting + globals, not missing library.

### 6. Checkout / unpack-trees

| Path | Location | Notes |
|------|----------|-------|
| **Engine** | `sley-unpack-trees::twoway_merge`, `checkout_two_way_engine` | Wired for checkout/switch/`reset --keep` |
| **Legacy routing** | Restore/reset matrix, porcelain errors | Spike: ~70–90 t-cells still on divergent paths (`docs/spikes/unpack-trees-engine.md`) |

### 7. Diff rename detection API

| Path | Location | Notes |
|------|----------|-------|
| **Legacy exact-only** | `diff_name_status_*_with_options` + `RenameDetectionOptions::default()` | `detect_inexact: false` preserves old behavior |
| **Extended** | `*_with_rename_options` | Opt-in inexact rename/copy |

Intentional compat layer; not dead code, but dual entry points increase test surface.

### 8. Tree flattening

| Path | Location | Notes |
|------|----------|-------|
| **Canonical** | `sley_diff_merge::flatten_tree` | Single implementation |
| **Wrappers** | `lib.rs`, `read_tree.rs` | Redundant delegates (deletable) |

### 9. Crate overlap candidates (not yet mergeable)

| Crates | Overlap | Merge? |
|--------|---------|--------|
| `sley-pretty` + `sley-ref-filter` | Both build on `sley-strbuf-expand`; different atom tables | **Future** — ADR Engine-epic C |
| `sley-diff-format` + `sley-diff-merge::render` | Color adapter bridge (`render_colors`); render lives in merge crate | **Keep split** — format is color/hunk helpers |
| `sley-fetch` + `sley-remote` | Pack install helpers vs full transport | **Keep** — `sley-fetch` is thin install seam |

---

## Feature Flag / cfg Branches

### Cargo features (intentional dual builds)

| Flag | Crate | Old vs new behavior |
|------|-------|---------------------|
| `http` | `sley-remote` | Without: SSH/local/git only; capability probes return false for HTTP/v2 |
| `remote` | `sley` facade | Optional `sley-remote` dep for embedders |
| `mmap` | `sley-odb`, `sley-cli`, `sley` | `#[cfg(feature = "mmap")]` vs heap read paths in ODB |
| `zlib-rs` / `pure-rust` | `sley-odb`, `sley-pack` | Compression backend selection (documented: not swappable at workspace root due to feature unification) |
| `fast-sha1` | `sley-core`, `sley-odb`, `sley-pack` | Hardware SHA-1 vs portable (byte-identical OIDs) |
| `tls-rustls` / `tls-native-tls` / `tls-platform-verifier` | `sley-transport`, `sley-remote`, `sley-cli` | TLS backend for HTTPS |
| `git-compat-i18n` | `sley-cli` | Optional `sley-i18n` shell script materialization |

### Compile-time capability flips (migration scaffolding)

```rust
// crates/sley-remote/src/capabilities.rs
pub const HTTP_PROTOCOL_V2_FETCH: bool = true;  // comment: "Flip once wired"
pub const SSH_CLONE_SUPPORTED: bool = true;
pub const BUNDLE_FETCH_SUPPORTED: bool = true;
```

**Status:** Features are wired; constants are **legacy indirection** — safe to simplify.

### Platform cfg (not migration debt)

- `#[cfg(unix)]` / `#[cfg(not(unix))]` — permissions, symlinks, procinfo (expected)
- `#[cfg(target_os = "macos")]` in `sley-worktree/filter.rs`, `sley-procinfo` — platform-specific

### Incomplete feature paths (keep until implemented)

| Branch | Where | Notes |
|--------|-------|-------|
| `protocol_v2: false` | `sley-remote/resolve.rs:94` for `git://` transport | git protocol still v0/v1 advertisement path |
| `#[cfg(not(feature = "http"))]` stubs | `fetch.rs`, `push.rs`, `ls_remote.rs`, `clone.rs` | Embedder build without HTTP |

---

## Documentation vs Reality Gaps

| Doc | Claims | Reality |
|-----|--------|---------|
| **`README.md`** | `cargo run -p git-cli` | Package is `sley-cli`; binary is `sley` |
| **`docs/sley-remote-extraction.md`** | Shallow not implemented; HTTP v2 unsupported; logic in `lib.rs` | `sley-remote` exists; shallow `shallow_fetch: true`; HTTP v2 fetch wired; orchestration mostly extracted |
| **`docs/adr/0001-cli-layer-engines.md`** | `log_format.rs` 1,168 lines; `branch.rs` 8,887 lines; 56 parsers; zero CLI `Repository` use | `log_format` merged into `lib.rs`/`log/`; `branch.rs` now ~11k lines; 47 parsers; facade note partially superseded Jul 2026 |
| **`docs/spikes/unpack-trees-engine.md`** | `workspace.rs` hand-roll | **Superseded** — stub only; engine routing exists for two-way paths |
| **`PARITY.md` / `GOAL.md` / `GIT_PARITY_CHECKLIST.md`** | Target Git **2.54.0** | `sley_core::UPSTREAM_GIT_COMPAT_VERSION` = **"2.55.0"** (HTTP agent strings, `--version`) |
| **`crates/sley-remote/src/capabilities.rs`** | “Flip to true once …” | Already `true` |
| **`GIT_PARITY_CHECKLIST.md` Phase 1** | Alias expansion not implemented | Still `[ ]` — no `alias.*` dispatch in CLI |
| **`TRACKER.md` #58** | CLI bypasses commit-graph | Partially fixed for merge-base/rev-list/log; `ancestor_depths`/`walk_commits` still widespread |

---

## Recommended Deletion/Migration Order

Ordered for **low risk → high leverage**, keeping the tree green between steps.

### Phase 0 — Documentation & dead stubs (days)

1. Fix `README.md` `git-cli` → `sley-cli`
2. Reconcile version pin: 2.54.0 docs vs `UPSTREAM_GIT_COMPAT_VERSION` 2.55.0
3. Archive/update `docs/sley-remote-extraction.md` and capability flip comments
4. Delete `workspace.rs` stub and redundant `flatten_tree` wrappers

### Phase 1 — Consolidate library seams (1–2 weeks)

5. Unify `ancestor_depths` → single `sley-rev` implementation (preserve shallow grafts)
6. Migrate remaining `merge_base` / push / notes callers off CLI-local copy
7. Finish submodule `.gitmodules` walks (13 sites → `sley-submodule`)
8. Move `virtual_ancestor_entry_map` into `sley-diff-merge`

### Phase 2 — `sley-options` wave (weeks, ADR Engine-epic A)

9. Pilot expansion: `tag`, `stash`, `worktree` (high parser count)
10. Migrate `log_validate_*` into typed option callbacks
11. Shared `diff_options` already exists — wire `log`/`show`/`format-patch` to it
12. Retire hand-rolled parsers command-by-command; delete `args.rs` mechanical duplicates as covered

### Phase 3 — Repository & CLI slimming (weeks)

13. Replace `GLOBAL_GIT_DIR` with explicit `RepositoryContext` threading (or setup-scoped guard)
14. Thin `remote_cmds.rs` to outcome formatters only
15. Adopt `sley::Repository` where repo-intrinsic boundary fits (`setup.rs` contract)

### Phase 4 — Format & unpack-trees (larger, ADR Engine-epic B/C)

16. Merge log + ref format atom dispatch atop `sley-strbuf-expand`
17. Centralize unpack-trees porcelain errors; finish restore/reset routing per spike doc
18. Extract log graph/notes driver from `lib.rs` into format engine crate module

### Phase 5 — Optional compat tightening (only with explicit decision)

19. **Do not remove** legacy remote files / deprecated config forms without parity sign-off
20. Consider feature-gating `augment_with_legacy_remote_files` for non-git-compat embedders only

---

## Deletion Risk Summary

| Risk | Items |
|------|-------|
| **Safe (S)** | Doc fixes, stubs, thin wrappers, stale const flips, i18n optional path |
| **Moderate (M)** | Parser migration, ancestry unification, submodule walks, `remote_cmds` thinning, rename API fold |
| **High (L)** | `GLOBAL_GIT_DIR` removal, log format extraction, unpack-trees error unification, legacy Git file compat |
| **Not deletable** | Protocol v0/v1, index v2, legacy remotes, `crlf` alias, exact-only rename defaults |

---

## Metrics (current tree)

| Metric | Value |
|--------|-------|
| `sley-cli/src/lib.rs` lines | 13,437 |
| `sley-cli/branch.rs` lines | 10,974 |
| `sley-cli/remote_cmds.rs` lines | 13,140 |
| `sley-diff-merge/src/lib.rs` lines | 13,405 |
| `parse_*_options` functions in `sley-cli` | 47 |
| Files using `sley_options::` in `sley-cli` | 3 |
| `GLOBAL_GIT_DIR` / `discover_git_dir` refs | ~225 |
| `RepositoryContext` adopters | ~24 command files |
| `workspace.rs` | 1 line (stub) |

---

*Review method: ripgrep for legacy/compat/deprecated/fallback/shim/with_options/v1/v2; read of ADR 0001, TRACKER.md, GIT_PARITY_CHECKLIST.md, PARITY.md, remote-extraction doc, unpack-trees spike; hotspot file line counts.*