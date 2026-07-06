# Core & Storage Crates Review

**Scope:** `sley-core`, `sley-object`, `sley-odb`, `sley-mmap`, `sley-pack`, `sley-index`, `sley-archive`  
**Date:** 2026-07-05  
**Cross-references:** `01-security.md`, `02-performance.md`, `03-legacy-migration.md`, `04-rust-practices.md`

---

## Summary

The core/storage layer is **architecturally sound and deliberately layered**: `sley-core` (types/errors/crypto) → `sley-object` (object model) → `sley-pack` (pack format) → `sley-odb` (object database) with `sley-mmap` isolating the workspace's only `unsafe`, and `sley-index` / `sley-archive` as adjacent on-disk consumers. The stack shows strong git-parity intent, security hardening on untrusted pack input (`sley-pack` `bounded_inflate_reserve`), and thoughtful API seams (`ObjectReader`/`ObjectWriter`, `EncodedObject` vs typed `Commit`/`Tag`, zero-copy `TreeEntries`/`CommitRef`).

**Strengths:** Clear dependency DAG, optional `mmap`/`fast-sha1`/`zlib-rs` feature matrix, offset-based pack decode, LRU byte-budget caches, streaming pack install, audited mmap invariants, excellent unit-test density in `sley-object`/`sley-pack`/`sley-odb`.

**Main risks:**
1. **God files** — `sley-odb` (10,492 lines) and `sley-pack` (10,319 lines) are monolithic single-`lib.rs` crates mixing parse, I/O, caching, repack, and reachability.
2. **Performance debt** — O(N) OID prefix resolution, duplicate pack-index loading (`PackIndexViewData` vs heap `PackIndex`), mutex layering on every packed read, index reads always `fs::read` despite `sley-mmap::open_index`.
3. **Security gap** — `install_raw_pack_from_reader` streams unbounded packs (H1 in `01-security.md`); inflate bounds live in `sley-pack` but not uniformly applied downstream.
4. **Lint inconsistency** — `sley-odb`/`sley-pack` deny `expect_used`; `sley-core`/`sley-object`/`sley-index`/`sley-archive` do not (see `04-rust-practices.md`).
5. **`sley-core` scope creep** — 2,601-line `lib.rs` bundles OID/crypto, date formatting, trace2, signatures, and CLI exit taxonomy; fine for parity but heavy for a "core primitives" crate.

**Test coverage:** Strong in-library for object/pack/odb; `sley-index` has 22 unit tests + 1 integration test; `sley-mmap` has 4 tests; `sley-archive` has 2 tests (tar + zip64 trailer only); **no benches** for `sley-index` or `sley-archive`.

---

## Per-Crate Assessment

### sley-core

| Dimension | Assessment |
|-----------|------------|
| **Architecture** | Foundation crate with zero default deps (`fast-sha1` optional). Exports `ObjectId`, `ObjectFormat`, `GitError`, `BString`, `RepoPath`, `Signature`, `GitTime`, hashing, trace2. Dependency-free default build is excellent for embedders. |
| **Rust practices** | Workspace `unwrap_used=deny`; production `.expect` in `to_hex`/`ObjectId::write_hex` (infallible `String` writes). No `#![deny(expect_used)]`. Global `Mutex<Option<PathBuf>>` for `original_cwd` silently drops lock poison errors. |
| **Performance** | Hand-rolled SHA-1/SHA-256 with streaming `StreamingDigest`; `sha1_object_digest` avoids copying large bodies. `DateMode::render` and `strftime` allocate per call — acceptable for CLI, not hot-path. |
| **Security** | `trace2::redact_unsafe_urls` strips credentials from trace output; trace targets restricted to absolute paths. No crypto vulnerabilities noted; SHA-1 is required for git OID parity. |
| **Legacy/dead code** | `GitError::Exit(i32)` marked legacy alongside typed `Cli(CliExit, String)`. `DateMode` has 15+ variants for git date parity — not dead, but candidates for `sley-pretty` extraction long-term. |
| **Test coverage** | 29 unit tests in `lib.rs` — thorough on OID, signatures, `BString`, `FullName`, date/ident edge cases. No integration tests. |
| **API design** | Strong newtypes (`ObjectId`, `FullName`, `RepoPath`). `GitError` is ergonomic but string-heavy (noted in `04-rust-practices.md`). `Signature` byte-exact round-trip design is exemplary. `ByteString` vs `BString` duplication is mildly confusing. |

**Notable:** `ObjectId` stores 32-byte backing array for both SHA-1 and SHA-256; `hex_prefix_matches` enables nibble-wise prefix without full enumeration (but ODB doesn't use it for `resolve_prefix`).

---

### sley-object

| Dimension | Assessment |
|-----------|------------|
| **Architecture** | Clean single responsibility: git object model. Clear split between **byte-exact** (`EncodedObject`, `parse_framed_object`) and **canonical typed** views (`Commit`, `Tag`, `Tree`). `TreeEntries`/`CommitRef`/`TagRef` zero-copy iterators are well-designed. |
| **Rust practices** | Only depends on `sley-core`. No crate-level `expect_used` deny. Tree/commit/tag parsers are fallible throughout. |
| **Performance** | `TreeEntries` avoids name allocations; `tree_entry_cmp` implements git's directory-suffix sort correctly. `Commit::write`/`Tag::write` allocate; `Tag::write` preserves `raw_body` for OID stability — good tradeoff. |
| **Security** | Parses untrusted object bytes; size checks in `parse_framed_object` (declared vs actual body length). No inflate here (ODB/pack own decompression). |
| **Legacy/dead code** | `tree_entry_object_type` is a coarse classifier (gitlink → `Commit`) — documented; `EntryKind` is the precise write-side API. No obvious dead modules. |
| **Test coverage** | **24 unit tests** — excellent: gpgsig/mergetag preservation, non-UTF-8 headers, tree builder canonical order, `TreeEntryRef` pointer stability, tag `raw_body` OID preservation. Best-tested crate in this group. |
| **API design** | Documented contract: `Commit::write` **drops** unknown headers/signatures; `Tag::write` **preserves** raw body. Callers must choose API deliberately — docs are clear. `TreeBuilder` is ergonomic for index/checkout writers. Re-exports `BString` from core — fine. |

---

### sley-odb

| Dimension | Assessment |
|-----------|------------|
| **Architecture** | Central object-database crate: loose objects, packfiles, multi-pack-index, alternates, shallow/promisor, bundle install, repack, reachability, geometric repack. `ObjectReader`/`ObjectWriter` traits are the right abstraction boundary for `sley-archive`, `sley-rev`, `sley-diff-merge`. In-memory `ObjectDatabase` + filesystem `FileObjectDatabase` share traits. |
| **Rust practices** | `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]` — gold standard. **10,492-line monolith** — worst god-file in storage layer. Heavy `Arc<Mutex<...>>` nesting (7+ cache maps). |
| **Performance** | **Critical issues** (see `02-performance.md`): `resolve_prefix` → `object_ids()` full scan; `cached_pack_index` heap-reads and materializes `Vec<PackIndexEntry>` bypassing mmap `PackIndexViewData`; mutex on every packed read; corruption fallback reparses all packs. **Positives:** offset-based decode via `sley-pack`, LRU byte-budget caches (96 MiB default, env-tunable), `present_loose_fanouts`, pack registry `recent_pack` hint, streaming `install_raw_pack_from_reader`, `verify_reads` off by default. |
| **Security** | Pack inflate bounded in `sley-pack`; ODB delegates decode. **H1:** `install_raw_pack_from_reader_with_options` has no max pack size — disk/CPU DoS from malicious remote. Loose-object reads fully inflate into memory. |
| **Legacy/dead code** | `ObjectDatabase` in-memory store coexists with `FileObjectDatabase` — both needed. Repack/geometric/cruft APIs are large but active. No separate dead modules (everything in one file). |
| **Test coverage** | **67 unit tests** in `lib.rs` — strong on prefix resolution, pack install, alternates, shallow, promisor, repack. Benches exist (`sley-bench`: `pack_install`, `cat_file`, `batch_check_profile`) but prefix scaling not stress-tested. |
| **API design** | Very broad public surface (~50+ free functions for reachability/repack/bundle). `FileObjectDatabase` is the production type but shares no trait with itself for "repository handle" — callers use concrete type. `grafted_parents` shallow seam is clean. `RawPackInstaller` trait enables test doubles. Feature `mmap` optional but CLI enables it; `cached_pack_index` path ignores mmap benefits. |

**Dependency note:** `sley-pack` pinned with `path = "../sley-pack"` because workspace `default-features` inheritance can't express zlib backend mutual exclusion — documented in `Cargo.toml`.

---

### sley-mmap

| Dimension | Assessment |
|-----------|------------|
| **Architecture** | **Exemplary isolation** of workspace `unsafe`. Single `MappedFile` type with typed entry points (`open_pack`, `open_index`, `open_multi_pack_index`, `open_commit_graph`) documenting git's atomic-rename immutability invariant. |
| **Rust practices** | Local `unsafe_code = "allow"`; `unwrap_used = "deny"`. Every `unsafe { Mmap::map }` has SAFETY comment. Symlink/non-regular-file rejection on all safe entry points. |
| **Performance** | Eliminates heap copy for large pack/index files when used. Fallback to `fs::read` in ODB on mmap failure is pragmatic. **Underutilized:** `sley-index` never calls `open_index`; `cached_pack_index` bypasses `open_pack` for `.idx`. |
| **Security** | Rejects symlinks and non-regular files — prevents mmap-based TOCTOU on unexpected path types. `open(path)` is `unsafe` and documented; safe wrappers scope usage to git-immutable files. SIGBUS risk documented for truncation — mitigated by git write semantics. |
| **Legacy/dead code** | None. 260 lines, single file. |
| **Test coverage** | 4 unit tests (pack, multi-pack-index, commit-graph, rejection). No mmap truncation/SIGBUS test (understandably hard in CI). |
| **API design** | Minimal, correct. `Deref` to `[u8]` enables drop-in use. `open(path)` could be crate-private — only safe wrappers are needed externally. |

---

### sley-pack

| Dimension | Assessment |
|-----------|------------|
| **Architecture** | Native packfile reader/writer: `PackFile`, `PackIndex`, `PackIndexViewData` (zero-copy), MIDX, bitmaps, delta encode/decode, streaming write. Thread-local zlib (`INFLATE.with`) for decode hot path. Correctly separated from ODB (format vs storage policy). |
| **Rust practices** | `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]`. **10,319-line monolith** — second god-file. `RefCell` for thread-local inflate state. |
| **Performance** | Read path is highly optimized (offset decode, header-only reads, bounded inflate). Write path issues (see `02-performance.md`): `DeltaIndex::new` per sliding-window entry; full upfront OID hashing; `write_packed_from_parts` buffers all compressed payloads (streaming API exists but simple path doesn't use it). Parallel compression capped at 4 threads — sensible. |
| **Security** | **Strong:** `bounded_inflate_reserve` (64 MiB cap, 1032× expansion ratio), regression tests for delta size bombs. This is the reference implementation other crates should share. |
| **Legacy/dead code** | `PackFile` in-memory representation coexists with offset-based `read_object_at` — both needed for verify vs read paths. `pure-rust` / `zlib-rs` feature split is intentional, not legacy. |
| **Test coverage** | **90 unit tests** — excellent coverage of index v1/v2, delta chains, MIDX, bitmaps, inflate bounds, write round-trips. **No write benchmarks** in `sley-bench`. |
| **API design** | `PackWriteOptions` builder is clean. `PackIndexViewData` vs owned `PackIndex` duality is powerful but easy to misuse (ODB uses view for lookup, heap index for offset→oid — performance footgun). `PackInput` trait for streaming write is good. Public surface is large but cohesive around "pack bytes." |

---

### sley-index

| Dimension | Assessment |
|-----------|------------|
| **Architecture** | Index v2–v4 read/write, extended flags, cache-tree (`TREE`), split index, untracked cache, fsmonitor, intent-to-add. **Single source of truth** for gitlink/symlink predicates (`is_gitlink`, `gitlink_stat_verdict`) — excellent cross-crate contract documented in-file. |
| **Rust practices** | Workspace lints only; no `expect_used` deny. `OnceLock`/`Mutex` for env-cached racy settings. 3,462 lines in one file — large but smaller than ODB/pack. |
| **Performance** | `Index::for_each_path` and `BorrowedIndex` avoid full materialization — **underused**; `read_repository_index` always `fs::read` + full `Index::parse`. No mmap despite `sley-mmap::open_index` and `sley-worktree` depending on both crates. |
| **Security** | Parses untrusted index bytes from disk; checksum validation, version bounds, path length checks. Split-index link expansion reads alternate paths — bounded by file size. |
| **Legacy/dead code** | v2 writer paths retained for parity (per `03-legacy-migration.md` — keep). No dead modules. |
| **Test coverage** | 22 unit tests in `lib.rs` + **`tests/intent_to_add.rs`** (git byte-for-byte parity for `git add -N`). Good for ITA; sparse coverage for cache-tree, fsmonitor, split index edge cases compared to object/pack. |
| **API design** | `IndexEntry`/`IndexEntryRef`/`BorrowedIndex` mirror object crate patterns well. `Stage`, extended flags API is complete. `read_repository_index` is the main entry — should gain a `read_repository_index_mapped` or internal mmap for hot paths. Gitlink stat verdict defers HEAD resolution to `sley-diff-merge` — correct layering. |

---

### sley-archive

| Dimension | Assessment |
|-----------|------------|
| **Architecture** | `git archive` implementation: shared `write_archive_entries` tree walk, `ArchiveSink` trait, tar + zip backends. `ArchiveConvert` bridges to `sley-worktree` smudge/attributes. **`publish = false`** — CLI-internal, not a public crate product. |
| **Rust practices** | `#![allow(clippy::too_many_arguments, clippy::type_complexity)]` at crate root. Depends on 7 workspace crates — heaviest fan-in in this group. `write_tar_gz_archive_full` buffers entire tar in `Vec` before gzip — memory spike for large archives. |
| **Performance** | Tree walk reads each blob via `ObjectReader::read_object` (full decode). `smudge` compares converted vs original to preserve zero-copy `Cow::Borrowed` — good. Zip deflate per entry with store fallback. Tar builds full archive in memory for `.tar.gz` path. |
| **Security** | `normalize_prefix`/`normalize_strip_prefix` reject `..` and absolute paths — good. Archive pathspec validation before any output byte — matches git's fail-fast. No path traversal in prefix handling. |
| **Legacy/dead code** | Documented gap: `ident` filter not implemented. `export-subst` requires caller-provided formatter. |
| **Test coverage** | **Weak:** 1 tar integration-style test in `lib.rs`, 1 zip64 trailer unit test in `zip.rs`. No pathspec, export-ignore, smudge, or pax header tests. Relies on upstream git test suite indirectly. |
| **API design** | `ArchiveSink` + `ArchiveEntry` enum is clean shared abstraction. `ArchiveConvert::from_tree` vs `from_worktree` covers attribute sources. Many `write_*` function variants (`write_tar_archive`, `_with_convert`, `_full`) — could collapse behind options struct. Hard dependency on `FileObjectDatabase` in `write_*_full` limits embedders using custom `ObjectReader`. |

---

## Cross-Crate Issues

### 1. Layering and dependency flow

```
sley-core ← sley-object ← sley-pack ← sley-odb
                ↑              ↑
           sley-index    sley-mmap (optional → odb)
                ↑
         sley-archive (also: config, pathspec, worktree, odb)
```

- **Correct:** Object model does not depend on storage. Pack format does not depend on ODB.
- **Concern:** `sley-archive` sits at the top of the storage stack but is unpublishable and pulls half the workspace — appropriate for CLI parity, not for a minimal storage SDK.
- **Concern:** `sley-index` does not depend on `sley-mmap` even though `sley-worktree` uses both; mmap integration is duplicated at call sites rather than centralized in index read API.

### 2. Duplicate pack index representations

| Path | Representation | Used for |
|------|----------------|----------|
| `RegisteredPack::index()` | `PackIndexViewData` (mmap/bytes) | OID → offset lookup (binary search) |
| `cached_pack_index()` | `PackIndex` with `Vec<PackIndexEntry>` | Offset → OID (linear scan) |

Both caches coexist in `FileObjectDatabase`. The slow path undermines mmap investment (see `02-performance.md` §3).

### 3. Feature-flag matrix fragmentation

| Feature | sley-core | sley-pack | sley-odb | sley-cli/sley |
|---------|-----------|-----------|----------|---------------|
| `fast-sha1` | opt-in | forwarded | forwarded | enabled |
| `zlib-rs` / `pure-rust` | — | default zlib-rs | forwarded | default zlib-rs |
| `mmap` | — | — | opt-in | **default on** |

Embedders using `sley-odb` directly without `mmap` get heap reads even when OS would benefit from mapping. Document clearly.

### 4. Security boundary: trusted vs untrusted input

| Input | Parser | Hardening |
|-------|--------|-----------|
| Pack bytes (network) | `sley-pack` inflate | `bounded_inflate_reserve` ✓ |
| Pack install size | `sley-odb` stream | **No cap** ✗ (H1) |
| Index file | `sley-index` | Checksum, version checks ✓ |
| Loose object | `sley-odb` zlib | No explicit inflate bound at ODB layer (relies on framed size) |
| Object body parse | `sley-object` | Framed size check ✓ |

### 5. Lint policy split

Strong `deny(expect_used)` on `sley-odb`, `sley-pack` (untrusted parsers). **Missing** on `sley-core`, `sley-object`, `sley-index`, `sley-archive`, `sley-mmap` (mmap has `unwrap_used` only). Align per `04-rust-practices.md` P0.

### 6. God-file maintainability

| Crate | Lines | Submodules |
|-------|-------|------------|
| sley-odb | 10,492 | 0 (only `mod tests`) |
| sley-pack | 10,319 | 0 |
| sley-index | 3,462 | 0 |
| sley-core | 2,601 | `trace2` inline module |
| sley-archive | 1,595 + 543 zip | `mod zip` only |

` sley-remote` was extracted from CLI; **ODB and pack are the next extraction candidates** — e.g. `loose/`, `pack_cache/`, `repack/`, `reachability/`, `pack/read`, `pack/write`, `pack/index`, `pack/delta`.

### 7. Bench coverage gaps (storage-specific)

From `02-performance.md`:

| Crate | Benches | Gap |
|-------|---------|-----|
| sley-pack | read (cat_file, batch_check) | **No write/repack** |
| sley-odb | pack_install, rev_parse prefix | Prefix at 500 objs only |
| sley-index | **None** | No parse/for_each_path |
| sley-mmap | implicit | No on/off A/B |
| sley-archive | **None** | — |
| sley-core/object | **None** | — |

---

## Migration/Deletion Opportunities

### Safe / high-value

| Item | Location | Action | Risk |
|------|----------|--------|------|
| Unify `cached_pack_index` with `load_pack_index_data` | `sley-odb` | Route through mmap + `PackIndexViewData`; add reverse index for offset→oid | Low — perf fix |
| Add `read_repository_index` mmap path | `sley-index` | Use `MappedFile::open_index`; optional `BorrowedIndex` return | Low |
| Prefix search without full enumeration | `sley-odb` | Fanout-aware probe using `hex_prefix_matches` + per-pack indexes | Med — correctness critical |
| Extract `trace2` + `DateMode` from `sley-core` | `sley-core` | New `sley-trace` or move to `sley-pretty` | Med — many imports |
| Split `sley-odb` into modules | `sley-odb/src/` | `loose.rs`, `pack_cache.rs`, `install.rs`, `repack.rs`, `reachability.rs` | Low if move-only |
| Split `sley-pack` into modules | `sley-pack/src/` | `index.rs`, `read.rs`, `write.rs`, `delta.rs`, `midx.rs` | Low if move-only |
| Add `expect_used` deny | core/object/index/archive | Match odb/pack policy | Low |
| `sley-archive` test expansion | archive | Pathspec, export-ignore, smudge round-trip | Low |

### Do not delete (parity shims)

- Index v2 write paths, extended flags, cache-tree extensions
- Both `PackIndex` and `PackIndexViewData` representations (fix usage, don't delete)
- `ObjectDatabase` in-memory backend (tests, merge engine)
- `Tag::raw_body` preservation semantics
- `Commit` canonical write (drops signatures) — documented contract
- `zlib-rs` / `pure-rust` dual backend features

### Consolidation candidates (longer-term)

| From | To | Rationale |
|------|-----|-----------|
| `ByteString` in core | Deprecate in favor of `BString` | Two byte-string types |
| `GitError::Exit(i32)` | Remove after CLI migration | Legacy exit path |
| `write_tar_gz_archive_full` full tar buffer | Stream gzip | Memory on large archives |
| `sley-archive` hard `FileObjectDatabase` deps | Generic `ObjectReader` in `_full` APIs | Embedder flexibility |

---

## Priority Actions

### P0 — Correctness & security (this sprint)

1. **Add pack install size limit** on `install_raw_pack_from_reader_with_options` (H1, `01-security.md`) — config key mirroring `receive.maxInputSize` / `transfer.maxSize`.
2. **Fix OID prefix resolution** — replace `object_ids()` scan with fanout-aware prefix probe (`02-performance.md` P0 #1).
3. **Unify pack index loading** — `cached_pack_index` → `load_pack_index_data` + reverse index for offset→oid (`02-performance.md` P0 #2).

### P1 — Performance & consistency (next sprint)

4. **mmap index reads** — `read_repository_index` / `read_index_file_expanded` use `MappedFile::open_index`; promote `BorrowedIndex` to default for status/diff consumers.
5. **Reduce ODB mutex contention** — `parking_lot::RwLock` or sharded caches; don't hold `decoded` lock across decode (`02-performance.md` P0 #3).
6. **Add `expect_used = "deny"`** to `sley-core`, `sley-object`, `sley-index` (`04-rust-practices.md` P0 #3).
7. **Pack write `DeltaIndex` reuse** during sliding-window planning (`02-performance.md` P1 #6).

### P2 — Structure & quality (quarter)

8. **Split god files** — `sley-odb` and `sley-pack` into domain modules (compile-time win, reviewability).
9. **Bench expansion** — `pack_write`, `resolve_prefix` at 1k/100k scale, `Index::parse` vs `for_each_path`, mmap on/off (`02-performance.md` P2).
10. **`sley-archive` tests** — pathspec validation, export-ignore, tar pax long paths, smudge EOL.
11. **Extract `DateMode` / `trace2`** from `sley-core` when `sley-pretty` substrate matures (`03-legacy-migration.md` Phase 4).

### P3 — API polish

12. Document embedder feature matrix (`mmap`, `fast-sha1`, zlib backend) in crate READMEs.
13. Add `ObjectReader`-only variants of `sley-archive::_full` functions.
14. Replace `GitError::Exit` with `Cli` variant everywhere.

---

## Metrics

| Crate | `src` lines | Unit tests | Integration tests | `unsafe` | `expect_used` deny |
|-------|-------------|------------|-------------------|----------|-------------------|
| sley-core | 2,601 | 29 | 0 | 0 | No |
| sley-object | 1,602 | 24 | 0 | 0 | No |
| sley-odb | 10,492 | 67 | 0 | 0 | **Yes** |
| sley-mmap | 260 | 4 | 0 | 5 sites | No (unwrap only) |
| sley-pack | 10,319 | 90 | 0 | 0 | **Yes** |
| sley-index | 3,462 | 22 | 1 | 0 | No |
| sley-archive | 2,138 | 2 | 0 | 0 | No |

---

*Review method: full read of `Cargo.toml` and `lib.rs` (or chunked for large files); ripgrep for tests, unsafe, mmap, cache, TODO; cross-reference with sibling reviews in `reviews/2026-07-05/`.*