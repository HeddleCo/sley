# sley — Work Tracker

> Living tracker for the sley effort: enabling **heddle** to drop-in replace
> `gix`, decomposing the `sley-cli` monolith, strengthening domain types, and
> performance. **Last updated: 2026-06-05.**

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

## 🔄 In progress

### Network driver → new `sley-remote` crate  *(heddle's biggest remaining blocker)*
Lift fetch / push / clone / ls-remote **orchestration** out of the `sley-cli`
monolith into a callable library crate. The wire codecs (`sley-protocol`) and the
v2 pack encoder (`sley-pack`) are already public; what must move is the glue:
info/refs → advertisement parse → RPC, ref-map building, HEAD-symref resolution,
report-status validation, the credential hook, and the `file://`/local-path
object copy. Also: verify shallow `deepen <n>` is honored on HTTP fetch; wire the
TLS backend feature choice. This is also the first big extraction of the
`sley-cli` decomposition (#19).

---

## 📋 Backlog (prioritized)

1. **Config + facade polish** — global/system config read (`~/.gitconfig`,
   `/etc/gitconfig`) for `user.name`/`user.email` fallback; structured
   `[remote]` section edit (`remote add/remove/set-url`); `workdir()` /
   shallow-detect / `remote_names()` on the facade.
2. **Signatures & dates typing (#46)** — lossless `Signature` parse-*view* for
   commit/tag author/committer/tagger (raw bytes stay the source of truth for
   re-serialization); wire `GitTime`, **preserving git's `-0000` (tz-unknown)
   vs `+0000`** distinction.
3. **Dep-feature forwarding (#40)** — make ureq's TLS backend
   (rustls / native-tls / platform-verifier) and flate2's zlib backend
   downstream-selectable.
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
- **Remaining gaps:** network driver (lift), global/system config, structured
  remote edit, signatures/dates typing.

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
forwarding · `#46` strengthen domain types (signatures/dates/ref names/paths).
