# Sley 0.13.0 release notes

0.13.0 is a minor release. It contains #255, #254 and #257. It adds public API
in `sley-pack` and marks `PackLimitKind` `#[non_exhaustive]`, which is a
breaking change for code that matches it exhaustively. On 0.x that makes this
a minor bump from 0.12.1.

## Breaking change

- `sley_pack::PackLimitKind` is now `#[non_exhaustive]` and has a new
  `LiveBaseBytes` variant. Exhaustive `match` expressions on it need a wildcard
  arm.

## Iterative delta-chain reads (#255)

Delta-chain resolution on the targeted read path was recursive, so a deep chain
read on a small thread stack (for example a 2 MiB async worker stack)
overflowed. Every targeted read now runs on one heap-allocated work stack.

- New module `sley_pack::chain` with `resolve_delta_chain`, the single
  iterative driver, and the `DeltaChainResolver`, `DeltaChainBase` and
  `DeltaChainStep` types resolvers use to plug in storage, caching, limits and
  recovery.
- New `sley_pack::read_pack_entry_at` and `sley_pack::DecodedPackEntry`, which
  read one pack entry and resolve it against a caller-supplied base.
- `sley_pack::DeltaBase` is now public and `#[non_exhaustive]`. It names the
  immediate base of a packed delta: an OFS offset or a REF object id.
- `FileObjectDatabase::read_object` in `sley-odb` walks pack entries, packed
  copies and object lookups as frames of one iterative walk. Source order and
  fallbacks are unchanged: a corrupt copy still falls back to loose storage,
  redundant pack copies and alternates before the original error is reported.
- Behaviour change: a REF-delta cycle across packs, or an OFS entry that names
  itself as its base, now returns a `pack delta cycle detected` error instead
  of recursing until the stack is exhausted. The error takes part in the normal
  fallbacks, so a good copy elsewhere still satisfies the read.

## Sequential plan-driven pack scan (#254)

New `sley_pack` scan API for reading many objects from one indexed pack in a
single pass, in pack order, without inflating headers.

- `PackScan::from_slice` and `PackScan::new` inspect entry headers of an
  indexed pack (slice or positional source) without inflation.
- `PackScan::plan` closes a set of requested ids over their transitive bases and
  returns a `PackScanPlan`. `PackScanPlan::external_bases` lists the REF bases
  that live outside the pack.
- `PackScanPlan::cursor` returns a `PackScanCursor` that decodes each planned
  entry once, yielding `PackScanObject` values. External bases load lazily
  through a callback returning an `Arc<EncodedObject>`, and the scan honours
  cooperative cancellation.
- `ScanLimits` bounds delta depth, total materialization and retained bases
  independently. Defaults are 4095, 1 GiB and 128 MiB.
- `PackScanStats` reports entries and bytes inflated and peak live-base bytes.
- `PackScanEntry`, `PackScanKind`, `PackScanBase`, `ScanLimits`,
  `PackScanStats` and `PackScanObject` are the data types. The structs are
  `#[non_exhaustive]`, so construct `ScanLimits` through `ScanLimits::new`.
- Duplicate object ids resolve the same way as Git's index lookup.

## Checkout follow-ups (#257)

No public signatures change.

- Directory-removal diagnostics match Git: repository-relative paths, plain OS
  messages, a warning for every `rmdir` error except a missing directory, and
  silent cleanup of empty parent directories (including the public pruning
  helper).
- When opening a leaf directory fails, checkout and reset warn and skip it and
  carry on with later operations. The directory is kept and the
  descriptor-relative path-safety checks still apply.
- Sparse checkout runs through the case-collision pass, and worker parent-lock
  keys fold ASCII case, so `A/x` and `a/y` serialize parent creation.
- Lexical dot components are normalized for OS-path writers.
