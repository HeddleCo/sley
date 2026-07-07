# Branch State Review — Synthesis

**Date:** 2026-07-05  
**Branch:** `main` @ `250e1f54`  
**Scope:** Full codebase audit (not PR diff — on base branch)  
**Agents:** 8 parallel reviews → individual reports in this directory

---

## Executive Summary

sley is a **mature, security-conscious Rust git implementation** with strong engineering in the storage/transport layers and excellent CLI parity test coverage. There are **no critical security or correctness blockers** in the current tree.

The dominant technical debt is **architectural, not algorithmic**: ~286k lines in `sley-cli` still trap tier-3 behavior that ADR 0001 assigns to library engines, while several engine crates (`sley-protocol`, `sley-diff-merge`, `sley-odb`, `sley-pack`) have grown into **13k–14k line god files**. Performance and security gaps cluster around **unbounded remote input** and **O(N) object enumeration** paths that git avoids with fanout indexes.

**Verdict:** Safe to continue shipping parity work, but ROI is highest on **(1) hardening untrusted-input paths**, **(2) ODB prefix/index performance**, and **(3) CLI extraction waves** before adding more surface area.

---

## Finding Counts by Severity

| Severity | Security | Performance | Rust Practices | Legacy/Migration | Total (deduped) |
|----------|----------|-------------|----------------|------------------|-----------------|
| Critical | 0 | 0 | 0 | 0 | **0** |
| High | 1 | 5 | 3 | — | **~8** |
| Medium | 4 | 6 | 4 | ~10 migratable | **~20** |
| Low/Info | 7 | 6 | 8 | ~15 doc/stub | **~30** |

---

## Cross-Cutting Themes

### 1. Unbounded untrusted input (Security + Performance)

Multiple agents flagged the same seam:

| Issue | Locations | Fix |
|-------|-----------|-----|
| No fetch pack size cap | `sley-odb::install_raw_pack_from_reader`, `sley-fetch` | Add `fetch.maxInputSize` / mirror `receive.maxInputSize` |
| Unbounded inflate pre-alloc | `sley-diff-merge::git_patch_delta`, `plumbing::inflate_zlib_exact` | Share `sley-pack::bounded_inflate_reserve` |
| Unbounded credential helper read | `sley-transport::read_git_credential` | Cap at ~64 KiB |
| O(N) OID prefix resolution | `sley-odb::resolve_prefix` → `object_ids()` | Fanout-aware prefix search |

### 2. mmap underutilization (Performance + Core Storage)

- Workspace allows `unsafe` only in `sley-mmap` / `sley-procinfo` — **well isolated**
- CLI builds enable `mmap`, but `cached_pack_index` heap-reads `.idx` files anyway
- `sley-index` always `fs::read`s despite `MappedFile::open_index`
- `sley-rev` commit-graph **does** use mmap — pattern to extend

### 3. Lint policy vs reality (Rust Practices)

Workspace denies `unwrap_used` but **not `expect_used`**. `sley-cli` blanket-allows `clippy::all` + `unwrap_used`, making the workspace lint ineffective for the largest crate. Security-sensitive crates (`sley-pack`, `sley-protocol`, `sley-odb`) correctly use `#![deny(expect_used)]` — **extend this pattern**.

### 4. CLI as debt sink (Legacy + Integration)

| Metric | Value |
|--------|-------|
| `sley-cli` total LOC | ~286k |
| `lib.rs` | 13,437 |
| `remote_cmds.rs` | 13,140 |
| `branch.rs` | 10,974 |
| `parse_*_options` hand-rolled | 47 |
| `sley-options` adopters | 3–4 modules |
| `GLOBAL_GIT_DIR` / `discover_git_dir` refs | ~227 |
| Target CLI size (ADR 0001) | ~15–25k LOC |

**Not legacy (keep for git parity):** protocol v0/v1, legacy `remotes/` files, `crlf`↔`text` alias, deprecated config headers, exact-only rename API defaults.

### 5. God files blocking maintainability

| File | Lines | Suggested split |
|------|-------|-----------------|
| `sley-protocol/src/lib.rs` | 14,224 | v0/v1/v2/sideband modules |
| `sley-cli/src/lib.rs` | 13,437 | setup, log format, ancestry → engines |
| `sley-diff-merge/src/lib.rs` | 13,405 | merge_trees, patch, name_status, line_diff |
| `sley-cli/remote_cmds.rs` | 13,140 | thin formatters only |
| `sley-odb/src/lib.rs` | 10,492 | loose, pack, repack, reachability |
| `sley-pack/src/lib.rs` | 10,319 | index, delta, write, read |

### 6. Test distribution imbalance

- **Excellent:** 101 `sley-cli` integration tests (~74k LOC) — git oracle parity
- **Good:** `sley-protocol` (153), `sley-pack` (89), `sley-odb` (67), `sley-diff-merge` (122)
- **Gaps:** `sley-diff-format` (0 tests), `sley-procinfo` (0), `sley-index` (no benches), no diff/merge/pack-write benches

---

## Prioritized Action Plan

### P0 — Do first (days, high ROI)

1. **Fetch pack size limit** on `install_raw_pack_from_reader` (H1)
2. **Unify inflate bounds** in patch apply paths (M1/M2)
3. **Redact credentials** from user-visible URLs (FETCH_HEAD, prune, push errors)
4. **Add `expect_used = "deny"`** to workspace `Cargo.toml`
5. **Fanout-aware OID prefix search** — stop calling `object_ids()` for abbrev resolution

### P1 — Next sprint (1–2 weeks)

6. Route `cached_pack_index` through mmap `load_pack_index_data`; use reverse index for offset→oid
7. Cap credential-helper response size
8. Unify `ancestor_depths` → single `sley-rev` graph-aware implementation
9. Add unit tests to `sley-diff-format`
10. Delete safe stubs: `workspace.rs`, `flatten_tree` wrappers, stale capability flip constants
11. Fix docs: `README.md` `git-cli` → `sley-cli`, version pin 2.54 vs 2.55

### P2 — Extraction waves (weeks, per ADR 0001)

12. **`sley-options` fan-out** — tag, stash, worktree parsers (47 → shared tables)
13. **Split `sley-diff-merge/lib.rs`** — start with `merge_trees` + `patch` modules
14. **Thin `remote_cmds.rs`** to outcome formatters atop `sley-remote`
15. **Replace `GLOBAL_GIT_DIR`** with explicit `RepositoryContext` / `CliSession`
16. **Fold rename API** — `*_with_options` + `*_with_rename_options` → single family

### P3 — Structural (months)

17. Split `sley-protocol`, `sley-odb`, `sley-pack` god files
18. Unified format substrate (`sley-strbuf-expand` + `sley-pretty` + log engine in CLI)
19. mmap default for index reads; `BorrowedIndex` as default status/diff path
20. Library-level benches: pack write, diff-merge rename, prefix scaling, mmap A/B

---

## What NOT to Delete

These look like "legacy" but are **required git compatibility**:

- `augment_with_legacy_remote_files` (pre-config-file remotes)
- Deprecated `[remote.origin]` config header support
- Protocol v0/v1 alongside v2
- `crlf` attribute alias for `text`
- `RenameDetectionOptions` exact-only defaults (until callers migrated)
- Inherited shell surfaces: credential helpers, hooks, filter drivers

---

## Agent Reports

| Report | Focus |
|--------|-------|
| [01-security.md](./01-security.md) | Unsafe isolation, DoS, credentials, path traversal |
| [02-performance.md](./02-performance.md) | Hot paths, allocations, mutex contention, bench gaps |
| [03-legacy-migration.md](./03-legacy-migration.md) | Dual paths, deletion candidates, migration order |
| [04-rust-practices.md](./04-rust-practices.md) | Lints, god files, error types, test distribution |
| [05-core-storage.md](./05-core-storage.md) | core, object, odb, mmap, pack, index, archive |
| [06-network-transport.md](./06-network-transport.md) | protocol, transport, fetch, remote |
| [07-diff-worktree.md](./07-diff-worktree.md) | diff-merge, diff-format, worktree, sequencer |
| [08-cli-integration.md](./08-cli-integration.md) | CLI architecture, extraction roadmap |

---

## Positive Highlights

- Workspace `unsafe_code = "forbid"` with audited exceptions in mmap/procinfo
- `bounded_inflate_reserve` and pack delta hardening in `sley-pack`
- Strong pkt-line bounds and SSH arg validation
- Worktree path guards (`..`, absolute, symlink-beyond checks)
- `sley-object` exemplary test coverage and API documentation
- `sley-worktree` wave-47 decomposition as a template for diff-merge
- `deny.toml` + `cargo audit` in CI
- `sley-strbuf-expand` trait design (no proc-macro magic)
- Ref name newtypes in `sley-refs`

---

*Synthesized from 8 subagent reviews, 2026-07-05*