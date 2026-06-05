# sley — Work Tracker

> Living tracker for the sley effort: enabling **heddle** to drop-in replace
> `gix`, decomposing the `sley-cli` monolith, strengthening domain types, and
> performance. **Last updated: 2026-06-05 (bulk-read + SHA-1 + decoder perf, #51–#54).**

## Context

- **sley** — pure-Rust, minimal-dependency reimplementation of Git (target
  upstream 2.54.0), meant to be *used as a library and injected downstream*.
- **heddle** — a git-overlay VCS that wants to replace `gix` (gitoxide) with
  sley. It needs **byte-identical** git objects/refs/index and smart-HTTP
  transport, all callable as libraries. A non-`gix`-shaped API is fine (heddle
  adapts); "many smaller crates" is welcome. Its spec drives the gap list below.
- **Guiding type principle** — lossless newtypes + typed views; strict enums only
  for genuinely closed domains; never trade byte-exactness for ergonomics.

---

## ✅ Done

### Performance — git parity (committed: `0dcd291`..`1095004`)
All five perf levers landed; on a fresh packed repo sley beats git on
cat-file (~3.8×), log/rev-list (~2.6×), merge-base (~2.6×); status closed via
the stat-cache.

| # | Lever | Commit(s) |
|---|---|---|
| 1 | Index stat-cache (racy-git) for status/diff | `1095004` (prereq `0192d70`) |
| 2 | Offset-based single-object pack decode + bounded cache | `0238902` (+ `3de9f66`) |
| 3 | Commit-graph-accelerated history walks | `feadaed` |
| 4 | Diff: subtree-skip-by-OID + hot-path | `aad88ef` |
| 5 | Cut per-read object clones | (perf series) |

### Heddle gix-replacement audit
Five-cluster audit mapping heddle's required `gix` surface to sley. **Verdict:**
the byte-exact engine is present and interop-verified; the gap was the *library
facade* — much orchestration was locked inside the `sley-cli` monolith. Drives
the backlog below.

### Export round-trip core (heddle Flow 2) — `baab4d2`
Byte-exactness verified against the system `git` binary at every layer. Full
workspace test suite green (123 test binaries).

| Item | Crate(s) | New public API | Verified vs |
|---|---|---|---|
| Tree-editor | sley-object (+ facade) | `TreeBuilder`, `EntryKind`, `tree_entry_cmp`; `Repository::{edit_tree,write_tree,write_blob,write_object}` | `git write-tree` |
| Intent-to-add | sley-index | `IndexEntry::intent_to_add`/`is_/set_intent_to_add`/`stage()`, `Stage`, `INDEX_EXTENDED_FLAG_*`, `Index::upgrade_version_for_flags` | `git add -N` (byte-for-byte) |
| index_from_tree | sley-worktree (+ facade) | `index_from_tree(db,fmt,tree)`; `Repository::{index_from_tree,open_index}` | `git read-tree` + `ls-files --stage` |
| Ref CAS | sley-refs | `RefPrecondition` (Any/MustExist/MustNotExist/MustExistAndMatch/ExistingMustMatch), `FileRefTransaction::update_to` | unit (under-lock CAS, atomic batch) |
| ObjectId types | sley-core | `Ord`/`PartialOrd`, `FromStr`, `null`/`empty_tree`/`empty_blob`/`is_null` | known hashes |

---

### Network driver → `sley-remote` (Stages A–G)
fetch / push / clone / ls-remote are now callable `sley-remote` library APIs for
http / local / ssh, behind the `CredentialProvider` + `ProgressSink` seams;
shallow `--depth` clone/fetch is implemented and verified byte-for-byte vs system
git. Transport orchestration is fully out of the `sley-cli` monolith — the first
big slice of decomposition #19. Commits `a5e0d94`..`6c2f9f2`.

### zlib-ng backend (zlib half of #40)
Opt-in `zlib-ng` feature on sley-pack/sley-odb, forwarded through sley + sley-cli;
default stays pure-Rust (miniz_oxide). Commit `fcf2ebd`.

### Bulk object-read: profiled + fixed (#51) — `7bffad8`, `d40e5ba`
**Correction to the earlier #49 note:** the bulk-read gap was *not* rev-parse /
stdout formatting (that was an unprofiled guess). Profiling `cat-file --batch`
with macOS `sample` showed **~87% of read time in `sley_core::sha1`**:
`FileObjectDatabase` re-hashed every decoded object on *every* read to verify it
against the requested id — work git does only at index-pack/fsck. Two fixes:

- **Stop re-hashing on read** (`7bffad8`): trust the pack index / loose name;
  re-verification is opt-in via `SLEY_VERIFY_READS`, `validate`/fsck still hash,
  incoming packs still verified at index build. Byte-identical output.
- **Header-only `--batch-check`** (`d40e5ba`): `read_object_header_at` /
  `FileObjectDatabase::read_object_header` report type+size without inflating the
  body (base size from the pack header; delta result size from the delta stream's
  leading varint; loose framing only). Reusable cheap type/size primitive.

Measured (2,561-object / 302 MB packed repo, Apple Silicon, release):
`cat-file --batch` content 1.90s→0.27s (**14x→1.98x** of git); `--batch-check`
1.91s→0.081s (**~23x faster**, now ~4x of git, the residual being per-object CLI
rev-parse). With SHA-1 off the read path, `zlib-ng` now also helps (~13% content).
The win lives in the shared `read_object` path, so all read-heavy ops benefit —
including heddle's import/read path. Full workspace 1627 passed.

### Read/write hashing + decode follow-ups (#52–#54) — `1b4fc5f`..`d835e86`
On top of #51, four more measured wins (all byte-identical, full suite green):

- **Cached cache-budget env reads + reuse the open db in cat-file resolution**
  (`1b4fc5f`): budgets resolve once per process (OnceLock); the batch loop resolves
  through its already-open `FileObjectDatabase` instead of building one per line.
- **Streaming pure-Rust SHA-1 + opt-in `fast-sha1`** (`67531b2`, #52/#53): default
  SHA-1 streams header+body with no copy (compute-bound, ~3% on big blobs); the
  `fast-sha1` feature swaps in RustCrypto (ARMv8-SHA1 / x86 SHA-NI) — unsafe stays
  inside that crate. hash-object 200 MB: 896ms→229ms (3.9× scalar, 2.05× of git).
  Forwarded like zlib-ng; OIDs byte-identical (git == scalar == fast).
- **Thread-local inflate state** (`d835e86`, #54): reuse one `flate2::Decompress`
  instead of boxing a ~10 KiB `InflateState` per object across the read + bulk
  index-pack decode paths.
- **Opt-in mmap pack reads** (`c89b8c2`, #55): `mmap` feature memory-maps packs
  instead of heap-reading them. The one unavoidable `unsafe` (`Mmap::map`) is
  isolated in a new **`sley-mmap`** crate — the only crate not under the workspace
  `unsafe_code = "forbid"` — behind an audited safe wrapper (invariant: packs are
  written by atomic rename, never truncated in place → no SIGBUS). Off by default;
  a large-repo RSS / cold-start lever, ~parity on normal packs. Byte-identical.

Cumulative on the 2,561-obj / 302 MB repo: `cat-file --batch-check` **1.9s → 43ms
(~44×, now ~3× of git)**; `--batch` content **1.9s → 198ms (now ~1.8× of git)**.

### Large-repo parity profiling (vs git, on flutter — 93,729 commits)

Profiled `sley` against system git on a *real large repo* (small-repo benchmarks
had hidden these). Single-object/startup paths beat git; history- and worktree-
walking commands are far behind, and the gaps are **algorithmic, not parallelism**.

| command | sley vs git | cause |
|---|---|---|
| `rev-parse` | 4× faster ✓ | startup |
| `cat-file -p` (1 obj) | 2.4× faster ✓ | — |
| `cat-file --batch` | 1.8× slower | miniz inflate |
| `cat-file --batch-check` | 3× slower | per-object CLI rev-parse |
| `ls-files` | 2× slower | index format |
| `rev-list --count` | 7× slower | walk reads every commit object |
| `merge-base` | 14× slower | same walk |
| `log --format=%H` | 27× slower | `walk_commits` decompresses every commit |
| `status` | 135× slower | dup attr walk + full HEAD-tree read + re-hash |
| `diff <rev> <rev>` | **>750× (hung)** | **mis-parsed as pathspecs → whole-worktree rescan** |

**Fixes (filed #56–#59):**
- **#56 diff `<rev> [<rev>]` / ranges — DONE** (`bc671e9`): positional revisions
  were dropped (treated as paths), so `diff A B` silently did an index-vs-worktree
  diff (wrong output + full-worktree rehash). Now routes to tree-vs-tree /
  tree-vs-worktree / tree-vs-index. flutter `diff HEAD~1 HEAD`: >15s → 174ms,
  byte-identical to git. New regression test.
- **#57 status (135×)** — multi-part: status does *two* full worktree walks (one
  for entries, one to build the attribute matcher across 3,871 dirs for 4
  `.gitattributes`, no filters configured) + a full HEAD-tree read (no cache-tree)
  + per-file hashing where the stat-cache misses. Fold attributes into the single
  traversal; use the index cache-tree for index-vs-HEAD; confirm the stat-cache
  shortcut engages on a git-written index.
- **#58 log/rev-list/merge-base (7–27×)** — `walk_commits` reads+parses every
  commit object; the commit-graph (`CommitGraphContext`, already parses flutter's
  graph) is wired only into `^`/`~` nav, not the main walk. Source parents +
  commit-time from the graph; read objects only when full commit data is needed.
- **#59 abbrev length** — git auto-grows short-OID width on huge repos; sley
  hardcodes 7. Cosmetic-but-real; affects diff index lines, `log --abbrev`, etc.

## 🔄 In progress

_(#57 status + #58 commit-graph walk — the remaining two parity refactors.)_

---

## 📋 Backlog (prioritized)

1. ~~Config + facade polish~~ **DONE** — global/system config read + identity
   fallback (env → -c → repo → global → system), callable `[remote]`
   add/remove/set-url + `remote_names`, and `workdir()`/`is_shallow()`/
   `remote_names()` on the facade. (Comment-preserving config round-trip still
   pending — writes reformat via `to_canonical_bytes`.) Commits `42d5db9`,`7135dde`.
2. ~~Signatures & dates typing (#46)~~ **DONE** — lossless `Signature` parse-view
   (raw bytes preserved, byte-exact round-trip; malformed → None) +
   `GitTime.negative_utc` for git's `-0000`. Commit `ff843b9`. Remaining
   (deferred): `FullName` ref-name newtype + path BString unification.
3. **Dep-feature forwarding (#40)** — zlib-ng backend DONE; `fast-sha1`
   (RustCrypto, hardware SHA-1) DONE; remaining: forward ureq's TLS backend
   (rustls / native-tls / platform-verifier) as selectable.
8. ~~mmap pack reads (#55)~~ **DONE** (`c89b8c2`) — opt-in `mmap` feature; the only
   `unsafe` is isolated in the new `sley-mmap` crate (the lone crate not under the
   workspace forbid). Off by default; large-repo RSS / cold-start lever.
4. **ObjectId `Copy` + `clone_on_copy` cleanup** — deferred from export-core
   (would add ~200 clippy warnings across 15 crates); do as one scoped
   `clippy --fix` pass.
5. **sley-cli monolith decomposition (#19)** — umbrella effort; the network
   driver is the first extraction. Then diff/log/commit/remote command families.
6. **Protocol v2 over HTTP** — deferred; v1 + `deepen` covers heddle.
7. **Smudge on restore/reset --hard/stash (#34)**; clean up 2 pre-existing
   `sley-cli` dead-code warnings (will fall out in decomposition).

---

## Heddle gap scorecard (condensed)

- **Byte-exactness: PROVEN** — live-oracle interop (real `git verify-pack` /
  `cat-file` / `ls-files` read sley output; upstream `t/*.sh` suite runs).
- **Ready (callable lib):** object read + transparent pack/delta decode; commit
  minting (`sley-sequencer`); blob write; index r/w v2/v3/v4; packed-refs; atomic
  ref txn + reflog; smart-HTTP codecs (v1+v2) + pack encoder; discover/open/init;
  ahead/behind revwalk; config include/includeIf.
- **Built (export-core):** tree-editor, intent-to-add, index_from_tree, ref CAS,
  ObjectId types.
- **Remaining gaps:** parallelism for large repos (#50), TLS-backend forwarding
  (TLS half of #40), comment-preserving config round-trip, and deferred ref-name /
  path newtypes. (mmap landed, opt-in, #55.) cat-file is now ~3×
  (`--batch-check`) / ~1.8× (`--batch`) of git, the residual being per-object CLI
  rev-parse (the library read path bypasses it). Bulk-read re-hash + header-only
  reads + streaming/fast SHA-1 + decoder reuse all landed; network,
  config/identity, and signature typing done.

---

## Type-strengthening status

- **Done (closed-domain enums / additive):** `EntryKind`, `Stage`,
  `RefPrecondition`; `ObjectId` `Ord`/`FromStr`/constants.
- **Pending:** `Signature`/`GitTime` parse-view (#46, lossless); `ObjectId`
  `Copy` (deferred, see backlog #4); `FullName` ref-name newtype + path
  BString unification (later).
- **Kept raw on purpose (lossless round-trip):** tree mode (`u32` + `EntryKind`
  classifier), index flags (`u16` + typed accessors), signature lines
  (`Vec<u8>` source of truth).

---

## Open task IDs

`#19` decompose sley-cli · `#34` smudge on restore/reset/stash · `#40` dep-feature
forwarding · `#46` strengthen domain types (signatures/dates/ref names/paths) ·
`#50` parallelism pass · `#51` bulk-read re-hash fix + header-only reads (done) ·
`#52`–`#54` streaming/fast SHA-1 + db-reuse + decoder-reuse (done) · `#55` opt-in
mmap via isolated `sley-mmap` crate (done).
