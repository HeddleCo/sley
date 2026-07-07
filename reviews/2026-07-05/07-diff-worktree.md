# Diff / Worktree / Sequencer Crate Group Review

**Scope:** `sley-diff-merge`, `sley-diff-format`, `sley-worktree`, `sley-submodule`, `sley-unpack-trees`, `sley-sequencer`  
**Date:** 2026-07-05  
**Total source:** ~49k lines across 6 crates (plus heavy `sley-cli` orchestration on top)

---

## Summary

This group is the heart of sley’s tree/index/worktree reconciliation and merge story. The engines are largely **correct and git-faithful**, but **decomposition lags badly in `sley-diff-merge`** (13.4k-line `lib.rs` — larger than pre-split `sley-worktree` was), and **CLI coupling remains the primary integration surface** (~700+ direct `sley_diff_merge::` / `sley_worktree::` references in `sley-cli`).

**Strengths**
- `sley-worktree` completed wave-47 mechanical split (21.8k → 10 modules); public API unchanged.
- `merge_trees` consolidated duplicated merge logic from CLI into one seam (documented at `lib.rs:7542`).
- `sley-unpack-trees` is a focused, testable port with clear `WorktreeProbe`/`WorktreeWriter` boundaries.
- `sley-submodule` is well-scoped with solid per-module unit tests (28 tests / ~1.2k LOC).
- `sley-diff-merge` has ~122 in-crate unit tests; rename/merge behaviour is additionally covered by extensive CLI parity tests (`diff_rename`, `merge`, `merge_tree`, `read_tree`, etc.).

**Highest-priority gaps**
1. **`sley-diff-merge/src/lib.rs` god file** — six distinct engines in one file; `merge_trees` alone is ~5.8k lines.
2. **Legacy dual API paths** — `*_with_options` vs `*_with_rename_options`; triple index-worktree diff paths (`BorrowedIndex` / `Index` / `read_index_snapshot`).
3. **`sley-diff-format` has zero tests** — pure formatting adapter with no unit coverage.
4. **Performance blind spots** — no `sley-bench` coverage for rename matrix or `merge_trees`; inexact rename is O(S×D) blob reads.
5. **CLI still owns orchestration** — `sley-cli/src/lib.rs` (13.4k) + `diff.rs` (3.9k) + `merge.rs` (5.4k) duplicate engine concerns per ADR 0001.

---

## Crate Inventory

| Crate | LOC (src) | Files | In-crate tests | Primary role |
|-------|-----------|-------|----------------|--------------|
| `sley-diff-merge` | **~20.4k** (lib.rs **13,405**) | 6 | **122** | Line diff, blob merge, name-status, patch apply, `merge_trees` |
| `sley-worktree` | **~29.8k** | 11 | **76** (75 in `lib.rs` test mod) | Checkout, status, index I/O, ignore, attributes |
| `sley-unpack-trees` | 1,719 | 1 | 21 | `oneway`/`twoway`/`threeway` tree→index engine |
| `sley-sequencer` | 1,919 | 3 | 17 | Rebase/replay todo machine, commit creation |
| `sley-submodule` | 1,404 | 5 | 28 | `.gitmodules` config, relative URL, move-head |
| `sley-diff-format` | 707 | 4 | **0** | Word-diff, colors, funcname adapter for renderer |

---

## 1. `sley-diff-merge` — God File & Dual Paths

### 1.1 God file decomposition

`lib.rs` is the largest single source file in the workspace. Only four submodules were extracted (`name` 700, `render` 3,224, `ws` 767, `range` 255). The remaining monolith contains **six separable domains** bounded by existing `// =====` section markers:

| Section (approx. lines) | Domain | Suggested module |
|-------------------------|--------|------------------|
| 17–110 | Gitlink resolution | `gitlink.rs` (or fold into `sley-submodule`) |
| 111–1098 | Myers / patience / histogram line diff | `line_diff.rs` |
| 1098–1587 | 3-way blob merge (`merge_blobs`, conflict styles) | `blob_merge.rs` |
| 1587–4210 | Name-status, tree diff, rename/copy detection | `name_status.rs` |
| 4210–5416 | Index/worktree entry helpers | `worktree_diff.rs` |
| 5417–7540 | Unified patch parse/apply/reject | `patch.rs` |
| 7542–10660 | `merge_trees` (merge-ort port) | `merge_trees.rs` |
| 10662–13405 | Tests (~2,743 lines) | `tests/` integration modules |

**Recommended wave:** mirror `sley-worktree` wave-47 — pure `use super::*` moves along existing seams, preserve every `sley_diff_merge::` path via `pub use`. Start with `merge_trees` (largest, most isolated) and `patch` (self-contained I/O).

`render.rs` (3,224 lines) is already extracted but is itself a decomposition candidate (hunk grouping vs emission vs color-moved).

### 1.2 Legacy dual code paths

#### `*_with_options` vs `*_with_rename_options`

`RenameDetectionOptions` exists explicitly for back-compat so struct-literal `DiffNameStatusOptions` callers keep compiling:

```1768:1786:crates/sley-diff-merge/src/lib.rs
/// so that existing callers — which build `DiffNameStatusOptions` with a struct
/// literal — keep compiling unchanged. Code that wants inexact detection uses
/// the `*_with_rename_options` entry points and this type instead.
// ...
    /// OID matches are detected, matching the legacy `*_with_options` behaviour.
    pub detect_inexact: bool,
```

There are **~15 public entry-point pairs** (`diff_name_status_head_worktree`, `diff_name_status_trees`, `diff_name_status_index_worktree`, `diff_name_status_tree_index`, …). Each `*_with_rename_options` wrapper delegates to `*_with_rename_options_and_diagnostics` or re-wraps `RenameDetectionOptions { base: options, detect_inexact: false, .. }`.

**Migration:** fold `detect_inexact`, thresholds, and `rename_limit` into `DiffNameStatusOptions` with `#[non_exhaustive]` + deprecated aliases; collapse to one function family. Cross-ref: `reviews/2026-07-05/03-legacy-migration.md` item on `RenameDetectionOptions`.

#### Triple index↔worktree diff path

`diff_name_status_index_worktree_changes` (line ~2289) branches three ways:

1. **`BorrowedIndex::parse`** → `diff_name_status_index_worktree_changes_for_borrowed_entries` (mmap/fast path)
2. **`Index` heap parse** → `diff_name_status_index_worktree_changes_for_entries`
3. **`read_index_snapshot`** → `diff_name_status_index_worktree_changes_from_snapshot` when ITA, unmerged stages, skip-worktree, or stat-cache metadata require full fidelity

Additionally, `diff_name_status_index_worktree_for_diff_files_with_options` is a **fourth variant** that augments with stat-dirty entries for `diff-files` semantics.

**Risk:** bug fixes must be applied in 3–4 places; `for_borrowed_entry_chunk` and `for_entry_chunk` are near-duplicates (~50 lines each).

**Migration:** single internal `IndexWorktreeContext` enum (`Borrowed` / `Owned` / `Snapshot`) with shared chunk walker; keep thin public wrappers only where API stability demands.

#### Stranded merge-recursive logic in CLI

`merge_trees` lives in the crate, but **`virtual_ancestor_entry_map`** and `cmd_merge_recursive` remain in `sley-cli/src/commands/merge_rebase/merge.rs` (~5.4k lines). ADR 0001 explicitly calls this out. The recursive strategy duplicates flatten/merge primitives already in `sley_diff_merge::merge_trees`.

### 1.3 Performance — rename detection & merge

#### Inexact rename/copy (`-M`/`-C`)

`detect_inexact_renames` builds a full source×destination scoring matrix after fetching all candidate blob bytes:

```3936:3948:crates/sley-diff-merge/src/lib.rs
    // git's `too_many_rename_candidates`: if the rename matrix would exceed a
    // `rename_limit` square, skip inexact detection wholesale
    if rename_limit_exceeded(
        deleted_indices.len(),
        added_indices.len(),
        options.rename_limit,
    ) {
        return changes;
    }
    // Fetch blob bytes only after the cap check
```

- **Complexity:** O(S×D) `blob_similarity` calls; each call does span-hash counting over full blob content.
- **Mitigation present:** `rename_limit²` cap skips matrix entirely; blob reads deferred until after cap check.
- **Gap:** no `sley-bench` benchmarks; large rename storms (e.g. `git diff -M` on directory reshuffles) are unmeasured.
- **Opportunity:** basename pre-filter (`basename_rename_matches`) exists but inexact path still scores all pairs; git uses more aggressive candidate pruning.

#### `merge_trees` (merge-ort)

~5,800 lines: flatten three trees, detect renames, directory renames, rehome, D/F conflicts, per-path diff3. Multiple `BTreeMap` clones (`entry_map_as_tracked`, `flatten_tree` per side). No bench coverage.

**Hot paths to watch:**
- `detect_merge_renames` + `apply_directory_renames` on wide trees
- Per-conflict `merge_blobs` + ODB reads in `merge_entry_maps`
- `write_merged_tree` recursive tree assembly

#### Line diff

Myers O(ND) with patience/histogram fallbacks — standard. Patience anchor search uses `HashMap<LineKey, Occ>`; histogram uses region scoring. Acceptable for typical file sizes; word-diff (`sley-diff-format`) adds another layer.

### 1.4 Test coverage

| Location | Tests |
|----------|-------|
| `lib.rs` `mod tests` | 91 |
| `render.rs` | 11 |
| `ws.rs` | 12 |
| `name.rs` | 5 |
| `range.rs` | 3 |

Strong on name-status edge cases (skip-worktree, ITA, sparse, gitlinks) and patch round-trips. **Weak on `merge_trees`** — only a handful of unit tests at the bottom of `lib.rs`; most merge-ort coverage is CLI integration (`merge_tree`, `merge`, `pull_rebase_conflict`).

**Gaps to add in-crate:**
- `merge_trees` rename/rename-delete/rename-rename collisions
- Inexact rename at `rename_limit` boundary
- `detect_inexact_copies` with `find_copies_harder`
- Directory rename + rehome (currently CLI-only)

### 1.5 CLI coupling

| CLI file | `sley_diff_merge::` refs | Notes |
|----------|--------------------------|-------|
| `lib.rs` | **164** | Patch rendering orchestration, submodule gitlink augmentation, rename thresholds |
| `diff.rs` | 94 | Porcelain diff driver |
| `diff_tree.rs` | 76 | Plumbing tree diff |
| `plumbing.rs` | 81 | `diff-tree`, `merge-tree`, patch family |
| `merge_rebase/merge.rs` | 55+ | Merge driver; recursive strategy still here |
| `format_patch.rs` | 21 | Uses `sley-diff-format` adapters |
| `diff_files.rs` | 31 | Index-worktree diff-files variant |
| `blame.rs` | 53 | Line-range / diff helpers |

The CLI **re-implements** concerns the engines should own: diff option normalization (`diff_options.rs`), per-file metainfo headers (renderer deliberately excludes these), submodule dirty augmentation (`augment_submodule_dirty_entries` in `lib.rs`), and merge-recursive virtual ancestors.

**Target state (ADR 0001):** CLI parses argv → calls `sley::` or engine `Options` structs → formats stdout. Today ~40% of diff/merge behaviour still lives in `sley-cli/src/lib.rs` patch pipeline.

---

## 2. `sley-diff-format` — Test Desert

707 lines across `words.rs` (523), `funcname.rs` (104), `hunks.rs` (71). **Zero `#[test]` blocks in the entire crate.**

The crate is a thin adapter:
- `WordDiffAdapter` implements `sley_diff_merge::render::HunkWordDiff`
- `render_colors` maps `DiffColors` → `RenderColors`
- `heading_classifier` wraps `CompiledFuncname`

All behavioural verification currently flows through CLI tests (`format_patch`, `diff` with `--word-diff`, `--color-words`). This is fragile: pure functions like `parse_color_value`, `push_colored_line`, and word-boundary logic are testable without a repository fixture.

**Minimum test pack (S effort):**
- `parse_color_value` for git color words and unknown tokens
- `WordDiffMode` plain/porcelain/color output on a fixed hunk
- `CompiledFuncname` / `default_funcname_heading` on sample C/Rust/Python lines
- `WordDiffAdapter` round-trip through a mock `HunkWordDiff` consumer

---

## 3. `sley-worktree` — Post-Split Residual Gods

### 3.1 Decomposition status

Wave-47 successfully split the former 21.8k `lib.rs` into 10 modules. `lib.rs` is now a **re-export hub + 2,400-line test module** (lines 61–2474). No production functions remain at crate root — good.

**Remaining god files:**

| File | LOC | Contents |
|------|-----|----------|
| `index.rs` | **4,201** | `add`/`update`/`refresh` index, split index, cache tree, `write_tree_from_index` |
| `checkout.rs` | 2,976 | Checkout, two-way/three-way worktree materialization |
| `status.rs` | 2,789 | Long/short status, untracked cache integration |
| `ignore.rs` | 2,934 | Exclude machinery |
| `filter.rs` | 2,579 | Pathspec/filter integration |
| `index_io.rs` | 2,008 | Atomic index write, stat refresh |
| `attributes.rs` | 1,625 | `.gitattributes` matching |
| `move_remove.rs` | 1,946 | `git mv`/`rm` index+worktree |

**Next split candidates:** `index.rs` along `add_*` vs `refresh_*` vs `write_*` seams; `checkout.rs` into `checkout_tree` vs `checkout_entry` vs conflict handling.

### 3.2 Cross-crate coupling

`Cargo.toml` depends on `sley-diff-merge` — used for diff-related status (submodule dirt constants, shared concepts). Worktree should not need the full merge engine long-term; gitlink/submodule dirt could move to `sley-submodule` or a thin `sley-gitlink` type.

### 3.3 Tests

76 tests total: **75 concentrated in `lib.rs` `mod tests`**, 1 in `ignore.rs`. Zero in-module tests for `checkout.rs`, `status.rs`, `index.rs` — the largest modules rely entirely on CLI integration (`checkout`, `status`, `add`, `restore`, `sparse_checkout`, etc.).

**Risk:** index/checkout regressions require full CLI fixture runs (~seconds each vs millisecond unit tests).

### 3.4 CLI coupling

| CLI file | `sley_worktree::` refs |
|----------|------------------------|
| `status.rs` | 66 |
| `index.rs` | 55 |
| `commit.rs` | 47 |
| `lib.rs` | 44 |
| `checkout.rs` | 34 |
| `plumbing.rs` | 66 |
| `read_tree.rs` | 20 |
| `reset.rs` | 19 |

`read_tree.rs` (2,583 lines) bridges `sley-unpack-trees` with worktree probes/writers — the adapter layer is still CLI-local rather than `sley-worktree::unpack` helpers.

---

## 4. `sley-unpack-trees` — Focused but Incomplete

Single `lib.rs` (1,719 lines) is **appropriate** for a line-faithful `unpack-trees.c` port. Clean trait boundaries:

- `WorktreeProbe` — uptodate / absent / submodule checks
- `WorktreeWriter` — apply phase
- Merge fns: `oneway_merge`, `twoway_merge`, `threeway_merge`, `bind_merge`

**21 unit tests** cover merge decision tables — good for a port crate.

**Outstanding upstream gaps** (5 `TODO(unpack-trees)` markers):
- Sparse directory (`S_ISSPARSEDIR`) merge arms in `twoway_merge` / `threeway_merge`
- Apply-phase D/F and ignored-file handling (partial)
- Real submodule worktree mutation (probe hooks exist via `sley-submodule::move_head`)

**CLI coupling:** concentrated in `read_tree.rs` (36 refs) + `checkout.rs` (1). Porcelain error strings are still duplicated between CLI and `reject_merge` paths (see legacy migration review).

**Decomposition:** not urgent. If it grows past ~2.5k lines, split `merge_fns.rs` from `driver.rs` / `check_updates.rs`.

---

## 5. `sley-submodule` — Healthy, Partially Adopted

Well-factored into `config`, `relative_url`, `update_strategy`, `move_head`. **28 unit tests** across modules — best test density in this group.

**CLI adoption incomplete:** `submodule.rs` is **4,430 lines** with only 2 PILOT migrations and an explicit TODO:

```
// TODO(submodule): migrate the other 13 `.gitmodules` walk sites
```

Hand-rolled section walks remain for init/sync/update/status paths. `sley-submodule` provides `SubmoduleConfigSet` but CLI still re-parses `.gitmodules` in many places.

**Integration points:**
- `sley-unpack-trees` → `check_submodule_move_head` / `verify_clean_submodule`
- `sley-diff-merge` → gitlink helpers (`gitlink_git_dir`, `gitlink_head_oid`) overlap with `move_head` concerns
- `read_tree.rs` (14 refs), `submodule.rs` (21 refs), `grep.rs` (8 refs)

**Opportunity:** move `gitlink_*` from `sley-diff-merge` into `sley-submodule` to break the worktree↔diff-merge dependency for submodule paths.

---

## 6. `sley-sequencer` — Adequate

1,919 lines across `lib.rs` (433), `rebase.rs` (867), `replay.rs` (619). Reasonable module split.

**17 tests** in `rebase.rs` / `replay.rs`. Commit creation (`create_commit`, `format_commit_identity`) is shared across merge/rebase/stash/am — good centralization.

**CLI coupling:** moderate and appropriate for a sequencer (state files + todo parsing stay CLI-adjacent):
- `replay.rs` (7), `rebase.rs` (10), `merge_rebase/merge.rs` (12), `stash.rs` (11), `commit.rs` (13)

**No god-file pressure.** Future work: fold `rebase.rs` todo-file I/O vs `replay.rs` action dispatch if either file grows past ~1.2k lines.

---

## Cross-Cutting: CLI Orchestration Map

```
┌─────────────────────────────────────────────────────────────────┐
│ sley-cli (lib.rs 13.4k, diff.rs 3.9k, merge.rs 5.4k,            │
│           read_tree.rs 2.6k, submodule.rs 4.4k)                 │
│  • argv → options structs (partially sley-options)              │
│  • patch metainfo headers, submodule augmentation               │
│  • merge-recursive / virtual_ancestor (stranded)                │
│  • unpack-trees WorktreeProbe/Writer adapters                   │
└────────────┬────────────────────────────────────────────────────┘
             │
    ┌────────┼────────┬──────────────┬─────────────┐
    ▼        ▼        ▼              ▼             ▼
diff-merge worktree unpack-trees  submodule   sequencer
    │        │            │           │           │
    └────────┴────────────┴───────────┴───────────┘
              diff-format (render adapter only)
```

**Dependency concern:** `sley-worktree` → `sley-diff-merge` creates a coupling where status/checkout pulls in the entire merge engine. Trimming this edge (gitlink/submodule only) would clarify layering.

---

## Prioritized Recommendations

| Priority | Item | Crate | Effort | Impact |
|----------|------|-------|--------|--------|
| **P0** | Split `lib.rs` — start with `merge_trees.rs` + `patch.rs` | diff-merge | **L** | Maintainability, reviewability, compile times |
| **P0** | Add unit tests to `sley-diff-format` | diff-format | **S** | Regression safety for word-diff/colors |
| **P1** | Unify index-worktree diff internal paths | diff-merge | **M** | Correctness drift prevention |
| **P1** | Fold `RenameDetectionOptions` into `DiffNameStatusOptions` | diff-merge | **M** | API surface reduction |
| **P1** | Move `virtual_ancestor_entry_map` + merge-recursive into diff-merge | diff-merge + CLI | **M** | ADR 0001 alignment |
| **P1** | Split `index.rs` (4.2k) into add/refresh/write modules | worktree | **M** | Mirrors wave-47 success |
| **P2** | Add `sley-bench` targets: inexact rename matrix, `merge_trees` wide tree | diff-merge | **M** | Performance regression detection |
| **P2** | Migrate 13 `.gitmodules` walks in `submodule.rs` | submodule + CLI | **M** | DRY config parsing |
| **P2** | Move `gitlink_*` helpers to `sley-submodule`; drop worktree→diff-merge dep | submodule, worktree | **M** | Layering |
| **P2** | Relocate worktree tests from `lib.rs` into per-module `#[cfg(test)]` | worktree | **M** | Test locality |
| **P3** | Extract `read_tree` probe/writer adapters into `sley-worktree` | worktree + CLI | **M** | Thinner CLI |
| **P3** | Complete sparse-dir `TODO(unpack-trees)` arms | unpack-trees | **L** | Sparse checkout parity |
| **P3** | Centralize unpack-trees porcelain errors | unpack-trees + CLI | **L** | Message parity |

---

## Test Gap Summary

| Crate | In-crate tests | CLI integration tests | Gap severity |
|-------|----------------|----------------------|--------------|
| `sley-diff-merge` | 122 | Extensive (`diff_*`, `merge*`, `am`) | Medium — `merge_trees` undertested in-crate |
| `sley-diff-format` | **0** | Indirect via `format_patch`, `diff --word-diff` | **High** |
| `sley-worktree` | 76 (concentrated) | Extensive (`checkout`, `status`, `add`, …) | Medium — module-local tests missing |
| `sley-unpack-trees` | 21 | `read_tree`, `checkout`, `reset` | Low |
| `sley-submodule` | 28 | `submodule` CLI suite | Low |
| `sley-sequencer` | 17 | `rebase`, `sequencer`, `replay` | Low |

---

## Conclusion

The engines are **functionally mature** (git parity tests pass broadly), but **structural debt is concentrated in `sley-diff-merge/src/lib.rs`** — it is the single highest-value decomposition target in the workspace. `sley-worktree` already proved the mechanical-split pattern works; applying it to diff-merge is the obvious next wave.

`sley-diff-format` is the standout **test hole**: small, pure, and currently unprotected. CLI coupling is expected during gap-closing but has ossified: `sley-cli/src/lib.rs` remains a second diff/merge engine for orchestration that ADR 0001 assigns to tier-3 crates.

Performance risk is **manageable** (rename limits exist) but **unmeasured** — add benches before optimizing inexact rename pruning or `merge_trees` map churn.