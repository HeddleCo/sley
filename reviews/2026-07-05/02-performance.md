# Performance Review

## Summary

sley’s hot paths show deliberate performance engineering in pack **read** and ODB **lookup/decode**: offset-based pack decoding, thread-local zlib, LRU byte-budget caches, header-only batch-check, bounded parallel compression, and streaming pack install. The largest remaining risks are **full-repository scans** for OID prefix resolution, **mutex layering** on every packed read, **duplicate index loading paths** (mmap view vs heap `PackIndex`), and **bench coverage gaps** that leave pack write, diff-merge, index, and library-level rev/refs paths largely unmeasured.

Default CLI/meta-crate builds enable `mmap`, but `sley-odb` keeps `mmap` off unless the feature is enabled; even with `mmap`, `cached_pack_index` bypasses the mmap loader.

---

## Critical / High Impact

### 1. OID prefix resolution scans the entire object store — O(N) per lookup

`resolve_prefix` delegates to `object_ids_with_prefix`, which calls `object_ids()` and linearly filters every OID in the repository (loose fanout walk + all pack indexes + alternates), then sorts.

```6217:6234:crates/sley-odb/src/lib.rs
    pub fn resolve_prefix(&self, prefix: &str) -> Result<ObjectPrefixResolution> {
        let mut matches = self.object_ids_with_prefix(prefix)?;
        Ok(match matches.len() {
            0 => ObjectPrefixResolution::Missing,
            1 => ObjectPrefixResolution::Unique(matches.remove(0)),
            _ => ObjectPrefixResolution::Ambiguous(matches),
        })
    }

    pub fn object_ids_with_prefix(&self, prefix: &str) -> Result<Vec<ObjectId>> {
        validate_object_id_prefix(self.format, prefix)?;
        let mut matches = Vec::new();
        for oid in self.object_ids()? {
            if object_id_matches_prefix(&oid, prefix) {
                matches.push(oid);
            }
        }
        Ok(matches)
    }
```

`object_ids()` itself walks the full `objects/` tree and every `.idx`:

```6176:6185:crates/sley-odb/src/lib.rs
    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        let mut oids = object_ids_in_objects_dir(&self.objects_dir, self.format)?
            .into_iter()
            .collect::<HashSet<_>>();
        for alternate in &self.alternates {
            oids.extend(Self::without_alternates(alternate, self.format).object_ids()?);
        }
        let mut oids = oids.into_iter().collect::<Vec<_>>();
        oids.sort_by_key(ObjectId::to_hex);
        Ok(oids)
    }
```

**Impact:** `rev-parse` on short prefixes, `fsck`, `count-objects`, and any abbreviated-OID CLI path become O(N) in object count. On a 1M-object repo this dominates interactive use. Git uses fanout-aware prefix probes on pack indexes and loose directories without enumerating all OIDs.

**Evidence in benches:** `rev_parse.rs` benchmarks `odb_resolve_prefix` against 500 objects — the regression would scale linearly and is not stress-tested.

---

### 2. Mutex contention on every packed object read

`FileObjectDatabase` layers many `std::sync::Mutex` caches: `pack_bytes`, `pack_indexes`, `pack_registry`, `decoded`, `pack_deltas`, `pack_header_types`, plus per-pack `delta_cache` and `header_type_cache` mutexes on `RegisteredPack`.

A single `read_packed_object_at_lookup` can acquire `decoded.lock()` twice (pre- and post-decode), then `pack_deltas.lock()` to get/create a per-pack cache, then the per-pack `delta_cache.lock()` on every delta-base step:

```6305:6375:crates/sley-odb/src/lib.rs
    fn read_packed_object_at_lookup(
        &self,
        oid: &ObjectId,
        pack_lookup: &PackLookup,
    ) -> Result<Arc<EncodedObject>> {
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(oid)
        {
            return Ok(object);
        }
        // ...
        let delta_cache = pack_lookup.delta_cache(self);
        // ...
        if let Ok(mut cache) = self.decoded.lock() {
            cache.put(*oid, Arc::clone(&object));
        }
        Ok(object)
    }
```

**Impact:** Parallel `cat-file --batch`, fetch unpack verification, and tree walks that fan out across threads will serialize on these mutexes. `parking_lot::RwLock` with read-mostly cache maps, shard-by-pack-path, or lock-free `Arc` swap for registry snapshots would reduce tail latency.

---

### 3. Duplicate pack-index loading: mmap view vs heap `PackIndex`

The registry hot path uses zero-copy `PackIndexViewData` via `load_pack_index_data` (mmap when enabled):

```5113:5127:crates/sley-odb/src/lib.rs
    fn index(&self, format: ObjectFormat) -> Result<Arc<PackIndexViewData>> {
        // ...
        let index_bytes = load_pack_index_data(&self.idx)?;
        let index = Arc::new(PackIndexViewData::parse_trusted_source_without_checksum(
            index_bytes,
            format,
        )?);
```

But `PackLookup::pack_index` routes through `cached_pack_index`, which always `fs::read`s and materializes a full `PackIndex` with `Vec<PackIndexEntry>`:

```6421:6431:crates/sley-odb/src/lib.rs
    fn cached_pack_index(&self, index_path: &Path) -> Result<Arc<PackIndex>> {
        // ...
        let index = Arc::new(PackIndex::parse(&fs::read(index_path)?, self.format)?);
```

This path is used for `pack_oid_at_offset` (ofs-delta fallback) with a **linear** `.entries.iter().find()`:

```6623:6635:crates/sley-odb/src/lib.rs
    fn pack_oid_at_offset(
        &self,
        pack_lookup: &PackLookup,
        offset: u64,
    ) -> Result<Option<ObjectId>> {
        match pack_lookup.pack_index(self) {
            Ok(index) => Ok(index
                .entries
                .iter()
                .find(|entry| entry.offset == offset)
                .map(|entry| entry.oid)),
```

**Impact:** Deep delta chains that hit ofs-delta cross-pack fallback pay: heap copy of entire index + O(entries) offset lookup, despite `PackReverseIndex` existing in `sley-pack`. Normal oid→offset lookup is fine (binary search on `PackIndexViewData`); offset→oid is not.

---

### 4. mmap not used consistently; off by default in `sley-odb`

`sley-odb/Cargo.toml` documents mmap as optional and **off by default**; `sley-cli`/`sley` meta-crates enable it. `load_pack_data` and `load_pack_index_data` mmap when the feature is on.

However:
- `cached_pack_index` ignores `load_pack_index_data` (see §3).
- Repository **index** reads always `fs::read` despite `MappedFile::open_index` in `sley-mmap`:

```1731:1732:crates/sley-index/src/lib.rs
) -> Result<Index> {
    let mut index = Index::parse(&fs::read(index_path)?, format)?;
```

**Impact:** Large packfiles and large indexes duplicate RSS on every `FileObjectDatabase` handle when mmap is disabled or bypassed. On macOS/Linux servers with multi-GB packs, enabling mmap uniformly is a significant win.

---

### 5. diff-merge rename detection clones full blob bodies

Rename/copy similarity fetches blobs via `read_object` and **clones** the entire body into a `Vec`:

```3145:3149:crates/sley-diff-merge/src/lib.rs
fn read_blob_bytes(db: &FileObjectDatabase, oid: &ObjectId) -> Option<Vec<u8>> {
    match db.read_object(oid) {
        Ok(object) if object.object_type == ObjectType::Blob => Some(object.body.clone()),
        _ => None,
    }
}
```

The inexact rename matrix is O(D×A) with git-compatible `rename_limit` guarding, but each pair scoring operates on owned `Vec<u8>`:

```4008:4024:crates/sley-diff-merge/src/lib.rs
    for (si, (_, src_bytes)) in deleted.iter().enumerate() {
        // ...
        for (di, (_, dst_bytes)) in added.iter().enumerate() {
            // ...
            let score = blob_similarity(src_bytes, dst_bytes);
```

**Impact:** `git diff` / `status` on trees with hundreds of renames can allocate and copy hundreds of blob bodies simultaneously. Using `Arc<[u8]>` or similarity on `&[u8]` with a deduplicated oid→bytes cache would cut allocator pressure.

---

## Medium Impact

### 6. Pack write: `DeltaIndex` rebuilt for every sliding-window entry

During delta planning, each object pushed into the window constructs a fresh `DeltaIndex` over the full base body:

```5572:5578:crates/sley-pack/src/lib.rs
        window.push_back(StreamingDeltaWindowEntry {
            base: StreamingCandidateBase::Current {
                idx,
                depth: plan[idx].depth,
            },
            object_type: objects[idx].body,
            index: DeltaIndex::new(&objects[idx].body),
        });
```

`DeltaIndex::new` allocates anchor vectors and bucket tables proportional to base size (`4926:4951:crates/sley-pack/src/lib.rs`). For window=10 and N objects this is O(N × window × base_scan) with repeated work when the same base stays in the window.

**Impact:** `git pack-objects` / repack on large repos. Git reuses `patch_delta` index structures across window entries. Consider caching `DeltaIndex` per object index for the duration of planning.

---

### 7. Pack write hashes every object before delta planning

`write_packed_impl` computes all OIDs up front:

```969:972:crates/sley-pack/src/lib.rs
        let mut object_ids: Vec<ObjectId> = Vec::with_capacity(objects.len());
        for object in &objects {
            object_ids.push(object.object_id(format)?);
        }
```

**Impact:** Full SHA-1/SHA-256 over every object body before compression/delta selection. Git can defer hashing for undeltified objects and hash delta results once. For large blob-heavy packs this is a full extra pass over all bytes.

---

### 8. Non-streaming pack write buffers all compressed payloads

`write_packed_from_parts` calls `compress_planned_payloads` to produce **all** compressed payloads before writing the pack header body (`1018:1019:crates/sley-pack/src/lib.rs`). Streaming APIs exist (`write_packed_from_source_to_writer`, `PACK_STREAM_COMPRESSION_WINDOW_OBJECTS = 256`) but the simple `write_packed` path materializes everything.

**Impact:** Peak memory ≈ sum of compressed payloads + delta buffers for the whole input set. Large `git push` pack generation should prefer streaming writers exclusively.

---

### 9. Corruption fallback rescans and reparses every other pack

`read_packed_object_from_other_packs` reads the pack directory, `fs::read`s each `.idx`, parses `PackIndex`, and attempts decode — bypassing all caches:

```6587:6618:crates/sley-odb/src/lib.rs
    fn read_packed_object_from_other_packs(
        &self,
        oid: &ObjectId,
        exclude: &PackLookup,
    ) -> Result<Option<Arc<EncodedObject>>> {
        // ...
        for entry in entries {
            // ...
            let Ok(idx_bytes) = fs::read(&idx_path) else {
                continue;
            };
            let Ok(index) = PackIndex::parse(&idx_bytes, self.format) else {
                continue;
            };
```

**Impact:** Rare on healthy repos; expensive on bitrot recovery or fuzzing. Should reuse `PackRegistrySnapshot` and mmap index views.

---

### 10. Index read path lacks mmap; full parse for most consumers

`read_repository_index` reads the entire index into a `Vec` and parses into owned `IndexEntry` structs. `Index::for_each_path` exists for path-only walks without materializing entries (`561:627:crates/sley-index/src/lib.rs`), but most worktree/status paths call full `parse`.

**Impact:** 100k-entry indexes (monorepos) pay full parse + allocate on every `status`/`diff` that opens the index. mmap + `BorrowedIndex` should be the default read path.

---

### 11. refs: repeated `fs::read` of `packed-refs` without handle-level cache

`list_refs_with_prefix` reads and parses `packed-refs` on every call:

```1122:1128:crates/sley-refs/src/lib.rs
        let packed_path = self.storage_dir.join("packed-refs");
        if packed_path.exists() {
            for packed in
                parse_packed_refs_with_prefix(self.format, &fs::read(packed_path)?, prefix)?
            {
                refs.push(packed.reference);
```

**Impact:** `for-each-ref` in loops (post-receive hooks, CI) re-parses the same file. A cached `Arc<[u8]>` or mmap with mtime invalidation would help.

---

### 12. Loose object reads allocate full inflated body

Loose reads `fs::read` compressed bytes, inflate entirely into `framed`, then parse (`7818:7855:crates/sley-odb/src/lib.rs`). No mmap; no header-only fast path reuse for full reads (header path exists separately for batch-check).

---

## Low / Optimization Opportunities

### 13. `PackIndexView::oid_bytes_at` clones `Range` on each access

Hot index accessors call `self.slice(entry_table.clone())` per lookup (`1716:1728:crates/sley-pack/src/lib.rs`). `Range<usize>` is cheap to clone, but this pattern appears in binary-search loops — storing resolved sub-slice references in the view would shave branches in tight fanout search.

### 14. String conversions on filesystem enumeration

Loose-object collection uses `to_str()` on directory entry names and `ObjectId::from_hex` with `format!` per file (`5504:5523:crates/sley-odb/src/lib.rs`). Parsing hex from `OsStr` bytes directly avoids UTF-8 checks and temporary `String` allocation.

### 15. `LruCache::touch` clones keys frequently

LRU list maintenance clones `K` on `attach_back`/`detach` (`4914:4922:crates/sley-odb/src/lib.rs`). For `ObjectId` and `u64` keys this is minor; for `PathBuf` delta-cache keys it could add up — prefer `u64` offset keys only (already done for delta cache).

### 16. diff-merge: extensive `path.clone()` in merge/rename plumbing

225 `.clone()` calls in `sley-diff-merge/src/lib.rs`; many are `entry.path.clone()` in tree comparison loops. `BString`/`Arc<Path>` sharing would reduce churn on wide trees.

### 17. rev-walk commit-graph: good mmap usage

`load_direct_commit_graph` uses `MappedFile::open_commit_graph` (`2805:2812:crates/sley-rev/src/lib.rs`) — positive pattern not yet applied to index/pack secondary paths.

### 18. Pack delta chain depth bound

`DEFAULT_PACK_DEPTH = 50` matches git; read path recursively walks chains with caching. Depth 50 × cold cache × mutex lock per link is bounded but still costly for pathological packs — consider iterative chain walk with explicit stack to avoid deep recursion (correctness unchanged).

---

## Positive Patterns

| Pattern | Location | Benefit |
|--------|----------|---------|
| Offset-based pack decode (`read_object_at_inner`) | `sley-pack:4343+` | Never parses whole pack per object |
| Thread-local zlib decompressor (`INFLATE.with`) | `sley-pack:4133+` | Avoids allocator churn on inflate |
| Header-only `read_object_header` + type cache | `sley-odb:6246+`, `sley-pack:4458+` | `cat-file --batch-check` without body inflate |
| LRU byte-budget caches (96 MiB default) | `sley-odb:4807+` | Size-aware eviction, env-tunable |
| `verify_reads` off by default | `sley-odb:4866+` | Skips re-hash on every read |
| Binary search `PackIndex::find` / view find | `sley-pack:1491+` | O(log N) oid lookup |
| Bounded parallel compression (max 4 threads) | `sley-pack:44-49` | CPU parallelism without unbounded memory |
| Streaming pack install / raw pack reader | `sley-odb:6115+` | Single-pass index+pack write |
| `present_loose_fanouts` | `sley-odb:5567+` | Skips 256 `ENOENT` probes on packed-only repos |
| Pack registry hint (`recent_pack`) | `sley-odb:6543+` | O(1) amortized pack selection on locality |
| Rename limit before blob fetch | `sley-diff-merge:3936+` | Avoids O(n²) I/O when limit exceeded |
| `Index::for_each_path` | `sley-index:561+` | Path walks without full entry materialization |
| `RevisionSpecRef` borrowed parse | `sley-rev:68+` | Allocation-free revision routing |

---

## Recommended Actions

### P0 — High ROI

1. **Prefix index:** Implement fanout-aware prefix search across pack indexes (and loose directories) without `object_ids()`. Mirror git’s `find_unique_abbrev` / `for_each_object` prefix behavior.
2. **Unify pack index loading:** Route `cached_pack_index` through `load_pack_index_data`; use `PackIndexViewData` or reverse index for offset→oid; eliminate duplicate `PackIndex` materialization.
3. **Reduce lock granularity:** Replace global `Mutex<HashMap<...>>` caches with `parking_lot::RwLock`, or immutable `Arc` snapshots swapped on miss; avoid holding `decoded` lock across decode (already done — extend to delta cache map).
4. **mmap by default for CLI builds:** Ensure all ODB index/pack paths use mmap helpers; add `open_index` to `read_repository_index`.

### P1 — Measurable improvements

5. **diff-merge blob cache:** `Fn(&ObjectId) -> Option<Arc<[u8]>>` for rename/copy detection; eliminate `body.clone()`.
6. **Pack write `DeltaIndex` reuse:** Cache indexes per object id for window lifetime; profile repack with 100k objects.
7. **refs packed-refs cache:** mtime-checked `Arc` bytes at `FileRefStore` level.
8. **Index mmap + `BorrowedIndex`:** Default status/diff to borrowed parse; full `Index` only when mutating.

### P2 — Bench coverage (see gaps below)

9. Add criterion benches (library-level, not CLI subprocess) for:
   - `PackFile::write_packed_with_options` (varying window/N)
   - `read_object` / `read_object_header` with deep delta chains (depth 10–50)
   - `resolve_prefix` vs full OID set (1k / 100k / 1M synthetic)
   - diff-merge rename detection (D×A matrix sizes)
   - `Index::parse` vs `for_each_path` on 10k–100k entry indexes
   - `FileRefStore::list_refs` with 1k refs
   - mmap on/off A/B on same fixture
   - concurrent `read_object` (rayon, 8 threads)
10. Scale fixtures beyond 500 objects / 200 commits; add multi-pack + MIDX fixture.
11. Add `pack_write` bench and `diff_merge` bench targets to `sley-bench/Cargo.toml`.

### P3 — Nice to have

12. Iterative delta chain resolution; offset→oid via pack reverse index.
13. Hex parsing from `OsStr` bytes in loose enumeration.
14. Document env vars (`SLEY_OBJECT_CACHE_BYTES`, `SLEY_DELTA_BASE_CACHE_BYTES`, `SLEY_VERIFY_READS`) in perf tuning guide.

---

## Bench Coverage Gaps

| Area | Current benches | Gap |
|------|----------------|-----|
| **sley-pack read** | `cat_file`, `batch_check_profile` (500 deltified blobs) | No deep chain, ref-delta, multi-pack, or MIDX |
| **sley-pack write** | None | No `write_packed` / repack / delta planning |
| **sley-odb install** | `pack_install` (fresh repo) | No re-install, MIDX, bitmap sidecars |
| **sley-odb prefix** | `rev_parse` (CLI + `resolve_prefix`) | Tiny fixture; doesn't expose O(N) scaling |
| **sley-rev** | `rev_list`, `rev_parse`, `tree_walk` — **CLI only** | No library `setup_revisions` / graph walk; 200 commits |
| **sley-refs** | `ref_walk` — **CLI only** | No `FileRefStore` / reftable; 10 branches |
| **sley-index** | None | No parse/write/refresh |
| **sley-diff-merge** | None | No diff/merge/rename |
| **sley-mmap** | Implicit via CLI default | No on/off comparison bench |
| **Concurrency** | None | Mutex contention invisible |
| **worktree** | `worktree_ops` (1k files, git compare) | Good pattern; extend to checkout |

Existing benches are well-structured (Criterion, throughput, component breakdown in `batch_check_profile`) but mostly subprocess CLI tests at small scale. Library-level benches would isolate regressions without fork+parse overhead.