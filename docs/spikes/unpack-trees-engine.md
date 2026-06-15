# Spike: completing the `unpack-trees` engine for upstream-git parity

**Status:** scoping (decision doc). No engine code changed by this spike.
**Branch:** `roadmap/unpack-trees` (base main `940bab9`).
**Oracle:** git 2.54.0 (PCRE) at `/tmp/git-pcre-prefix`. SHA-1.
**Author:** orchestrator scoping pass, 2026-06-15.

`unpack-trees` is git's core n-way tree→index→worktree merge engine
(`unpack-trees.c`, 3071 lines). It powers `read-tree -m`, `checkout` /
`switch` / `restore`, `reset` (`--hard` / `--merge` / `--keep`), the
worktree-application half of `merge`, sparse-checkout / `skip-worktree`, and
submodule-gitlink checkout. sley already has a **partial** port at
`crates/sley-unpack-trees/src/lib.rs` (1127 lines, a single file). This doc maps
what it covers vs. git, measures the parity opportunity across the dependent
t-files, and proposes a staged completion plan.

---

## 1. Headline

- **745 currently-failing cells** across the candidate set (read-tree /
  checkout / switch / restore / reset / merge-into-worktree / sparse /
  submodule). Not all are pure unpack-trees gaps (see classification below),
  but the engine is the upstream prerequisite for the large majority.
- **Single biggest cluster: submodule gitlink checkout — 277 failing cells**
  across `t1013` (68), `t2013` (73), `t7112` (82), `t7406` (54). All of it is
  *downstream* of unpack-trees: the gitlink `merged_entry` arm + the
  `check_submodule_move_head` hook must run for these to even start passing.
  This is exactly why the earlier standalone gitlink-apply effort landed 0
  cells — it was building a downstream consumer of an engine path that does not
  yet fire end-to-end.
- The engine's **per-path merge logic is largely correct already**; the misses
  are almost entirely in the *plumbing around* the engine: index **stat-info**
  refresh, the `-u` `check_updates` apply (D/F directory handling, ordering),
  and the fact that the **checkout/switch/restore porcelain bypasses the engine
  entirely** with a hand-rolled re-implementation.

### Structural finding (the load-bearing one)

There is **no `checkout.rs` / `switch.rs` / `restore.rs`**. The porcelain lives
in `crates/sley-cli/src/commands/workspace.rs` (5462 lines) as `cmd_checkout`,
`cmd_switch`, `cmd_restore`, `cmd_reset` — and **none of them import
`sley_unpack_trees`**. Instead, `workspace.rs:1303 fn checkout_twoway_dirty`
hand-rolls ~150 lines of twoway-merge logic (its own carry/block/update
classification at lines 1356–1404, its own error strings at 1391–1413) that
*parallels* `twoway_merge` in the engine but diverges from it. `cmd_reset`
similarly does not route through `oneway_merge`. So the engine is exercised only
by `read-tree`, `stash`, and `replay`; the highest-traffic porcelain
(checkout/switch/restore/reset) runs a separate, less-faithful code path. The
single highest-leverage move in this whole plan is **routing that porcelain
through the engine** — it both fixes parity *and* deletes the divergent fork.

---

## 2. Gap table — git's `unpack-trees.c` vs sley's crate

`sley-unpack-trees` is a pure, I/O-free per-path merge-function port. Tree
contents arrive as flattened `path → (mode, oid)` maps (`FlatTree`/`FlatIndex`);
worktree state is read through the `WorktreeProbe` trait and mutated through
`WorktreeWriter`. The driver `unpack_trees()` walks the path union and dispatches
each slice to the chosen `MergeFn`.

| git capability | sley status | evidence (file:line) | which caller(s) / t-files need it |
|---|---|---|---|
| `oneway_merge` (1-tree fast-forward / reset read) | **done** | `lib.rs:341 oneway_merge` | read-tree `--reset`, reset `--hard` |
| `twoway_merge` (switch old→new, carry local adds) | **done (logic); not wired to porcelain** | `lib.rs:372 twoway_merge`; bypassed by `workspace.rs:1303 checkout_twoway_dirty` | read-tree -m 2-tree, checkout/switch |
| `threeway_merge` (trivial 3-way, emit stages 1/2/3) | **done (logic); aggressive done)** | `lib.rs:462 threeway_merge`, aggressive arm `lib.rs:544` | read-tree -m 3-tree, merge, reset --merge |
| `bind_merge` (`--prefix`, refuse overlap) | **done** | `lib.rs:434 bind_merge` | read-tree --prefix |
| `verify_uptodate` / `verify_absent` (overwrite / remove) | **trait surface done; FS impl lives only in `read_tree.rs`** | trait `lib.rs:166`; impl `read_tree.rs:446 ReadTreeWorktree` | every -u path; checkout porcelain has its *own* ad-hoc check |
| `check_submodule_move_head` gitlink hook | **trait + engine call done; real impl in `sley-submodule`** | engine call `lib.rs:650,668`; impl `sley-submodule/src/move_head.rs:125` | t1013, t2013, t7112, t7406 (the 277-cell cluster) |
| `check_updates` (apply: removals→writes) | **partial — flat, no D/F dir handling, no stat-info writeback, unconditional rewrite** | `lib.rs:849 check_updates` (writer.remove then writer.write_blob; no dir-vs-file, no `lstat` refresh) | read-tree -u, checkout -u, reset --hard |
| **index stat-info refresh** (write `lstat` mtime/size into kept/updated entries; racy-clean) | **MISSING** | engine carries only `(mode, oid)` — `CacheEntry` `lib.rs:55` has no stat fields; `check_updates` never re-stats | t1001/t1002 `check_cache_at … dirty`, refresh semantics |
| `verify_clean_subdirectory` (D/F conflict: replacing a dir with a file / vice-versa) | **MISSING (TODO markers)** | `lib.rs:411,475` `// TODO(unpack-trees): … D/F-conflict … collapse to the reject below` | t1012, t6400, t2025 `--no-overlay … D/F` |
| sparse / `SKIP_WORKTREE` (`apply_sparse_checkout`, `mark_new_skip_worktree`, sparse-dir entries `S_ISSPARSEDIR`) | **MISSING (TODO markers)** | `lib.rs:411,513,523,590` `S_ISSPARSEDIR → merged_sparse_dir` TODOs; no `skip_worktree` field on `CacheEntry` | t1011, t1091, t1092, t3602, t7002 |
| `df_conflict_entry` synthesis (D/F marker passed into merge fns) | **MISSING (always 0)** | `lib.rs:474` "sley does not synthesize a df_conflict_entry" | merge / checkout D/F edges |
| `o->reset` matrix (`UNPACK_RESET_NONE` / `_OVERWRITE_UNTRACKED`) | **partial — only the 2 values sley exercises** | `ResetType` `lib.rs:103`; comment "Only the two values sley exercises today are modeled" | reset `--keep` semantics (t7110) need finer reset modes |
| `o->preserve_ignored` / `o->dir` (ignored-file protection during apply) | **MISSING** | no `dir`/ignore plumbing in crate | t1004 "clobbering an ignored file", checkout overwrite edges |
| porcelain error message catalog (`setup_unpack_trees_porcelain`, 8 error + 3 warning types) | **partial / divergent** | engine `reject_merge` `lib.rs:715` prints one fixed string; porcelain strings duplicated in `workspace.rs:1391–1413`, `read_tree.rs` | exact-text diffs in many cells |

**Gap summary (5 bullets):**

1. **The merge primitives are essentially done.** oneway/twoway/threeway/bind +
   the aggressive arm + the gitlink hook plumbing are faithful ports. The crate
   is *not* the bottleneck for the trivial-merge cells.
2. **Index stat-info is the missing substrate.** `CacheEntry` is `(mode, oid,
   stage)` with no `lstat` fields, and `check_updates` never re-stats written
   files. git's whole "is this path dirty / up-to-date / racy-clean" machinery —
   which `check_cache_at`, `diff-files`, and refresh all key on — has nothing to
   read. This single gap fails most of t1001/t1002 and ripples into reset/status.
3. **`check_updates` is too flat.** No directory-vs-file (D/F) handling, no
   `verify_clean_subdirectory`, no ignored-file protection (`o->dir`). It just
   removes-then-writes a flat path list.
4. **Sparse / `skip-worktree` is entirely unmodeled** (explicit TODO markers at
   every `S_ISSPARSEDIR` site). The sparse t-files also need the sparse-*index*
   format (a separate serialization concern), so only part of their misses are
   engine work.
5. **The porcelain doesn't use the engine.** checkout/switch/restore/reset in
   `workspace.rs` re-implement the merge by hand. Until they route through the
   engine, fixes to the engine don't reach the highest-traffic commands.

---

## 3. Measurement table

Oracle git 2.54 PCRE; `SLEY_BIN` = release build of `roadmap/unpack-trees` @
`940bab9`. Columns: ok / notok / total. **Floor** = enrolled parity floor (must
not regress). **Theme** = dominant failing-assertion cause (from the per-file
"failing assertions" section of `upstream-report.txt`), with a tag for the
primary *engine* gap each maps to.

| t-file | ok | notok | total | floor | failing theme → gap |
|---|---|---|---|---|---|
| t1000-read-tree-m-3way | 62 | 21 | 83 | – | trivial 3-way cells #45–#71 "must match / up-to-date" → **stat-info + verify_uptodate** |
| t1001-read-tree-m-2way | 4 | 25 | 29 | – | "carry forward local addition" + `check_cache_at … dirty` → **stat-info refresh** |
| t1002-read-tree-m-u-2way | 1 | 21 | 22 | – | same as t1001 with `-u` worktree apply → **stat-info + check_updates** |
| t1003-read-tree-prefix | 2 | 1 | 3 | – | leading-slash `--prefix` error text → **error catalog** |
| t1004-read-tree-m-u-wf | 8 | 9 | 17 | – | "clobbering an ignored file", D/F, funny symlink → **o->dir / D/F / check_updates** |
| t1005-read-tree-reset | 0 | 7 | 7 | – | "remove remnants from a failed merge", two-way reset → **oneway apply + check_updates** |
| t1008-read-tree-overlay | 1 | 1 | 2 | – | overlay multi-tree → **stat-info** |
| t1011-read-tree-sparse-checkout | 5 | 18 | 23 | – | sparse patterns / skip-worktree → **sparse** |
| t1012-read-tree-df | 1 | 4 | 5 | – | D/F resolve, D/F recursive → **verify_clean_subdirectory** |
| t1013-read-tree-submodule | 0 | 68 | 68 | – | `-u -m --recurse-submodules` gitlink checkout → **gitlink apply (submodule cluster)** |
| t2000-conflict-when-checking-files-out | 10 | 4 | 14 | – | checkout-index clobber → **verify_absent / check_updates** |
| t2003-checkout-cache-mkdir | 7 | 3 | 10 | – | mkdir on apply / D/F → **check_updates D/F** |
| t2006-checkout-index-basic | 7 | 2 | 9 | – | apply edges → **check_updates** |
| t2007-checkout-symlink | 2 | 2 | 4 | – | symlink apply → **check_updates (symlink)** |
| t2008-checkout-subdir | 6 | 3 | 9 | – | subdir pathspec apply → **check_updates** |
| t2011-checkout-invalid-head | 8 | 2 | 10 | – | error text edges → **error catalog** |
| t2013-checkout-submodule | 1 | 73 | 74 | – | `checkout <submodule>` index+worktree, ignore config → **gitlink apply (submodule cluster)** |
| t2014-checkout-switch | 4 | 0 | 4 | – | (passing) |
| t2020-checkout-detach | 16 | 10 | 26 | **16** | detach + reflog text edges → **porcelain (mostly non-engine)** |
| t2021-checkout-overwrite | 5 | 4 | 9 | – | overwrite untracked / D/F → **verify_absent / D/F** |
| t2022-checkout-paths | 1 | 4 | 5 | – | "do not clobber unrelated", unmerged, up-to-date, i-t-a → **porcelain→engine routing + stat-info** |
| t2023-checkout-m | 2 | 3 | 5 | – | `checkout -m` recreate conflicts → **threeway apply via porcelain** |
| t2024-checkout-dwim | 4 | 19 | 23 | – | dwim branch creation → **porcelain (mostly non-engine: ref dwim)** |
| t2025-checkout-no-overlay | 1 | 5 | 6 | – | `--no-overlay` delete-not-in-tree, D/F → **twoway via porcelain + D/F** |
| t2060-switch | 8 | 8 | 16 | – | detach suggestion, orphan, guess-create → **porcelain (mostly non-engine)** |
| t2070-restore | 4 | 11 | 15 | – | restore worktree/index/`--staged`/`--source` → **porcelain→engine routing** |
| t7102-reset | 37 | 1 | 38 | **37** | one residual (checkout -m dep) → near-complete |
| t7103-reset-bare | 10 | 3 | 13 | – | bare-repo guard text → **porcelain (non-engine)** |
| t7104-reset-hard | 1 | 2 | 3 | – | "restore unmerged", cache-tree → **oneway apply + stat-info** |
| t7110-reset-merge | 13 | 8 | 21 | – | `reset --keep` touch/no-touch classification → **reset-mode matrix** |
| t7112-reset-submodule | 0 | 82 | 82 | – | `reset --keep --recurse-submodules` gitlink → **gitlink apply (submodule cluster)** |
| t7113-post-index-change-hook | 1 | 3 | 4 | – | hook fired on index change → **check_updates + index writeback** |
| t6400-merge-df | 4 | 3 | 7 | – | F/D + modify/delete D/F → **verify_clean_subdirectory** |
| t6408-merge-up-to-date | 4 | 3 | 7 | – | `-s ours`/`-s subtree` up-to-date / ff → **twoway up-to-date** |
| t6424-merge-unrelated-index-changes | 7 | 12 | 19 | – | unrelated staged change must block → **verify_uptodate on index** |
| t6426-merge-skip-unneeded-updates | 2 | 11 | 13 | – | rename + skip-unneeded-update → **rename detection (NOT unpack-trees) + skip-update** |
| t1091-sparse-checkout-builtin | 22 | 55 | 77 | – | `sparse-checkout` subcommand + cone → **sparse config porcelain + sparse engine** |
| t1092-sparse-checkout-compatibility | 0 | 106 | 106 | – | sparse-*index* format / expansion → **sparse-index serialization (largely non-engine)** |
| t3602-rm-sparse-checkout | 3 | 10 | 13 | – | rm vs skip-worktree → **sparse / skip-worktree** |
| t7002-mv-sparse-checkout | 3 | 19 | 22 | – | mv vs skip-worktree → **sparse / skip-worktree** |
| t7400-submodule-basic | 70 | 54 | 124 | **70** | mixed; some gitlink-checkout → **partial gitlink cluster** |
| t7406-submodule-update | 16 | 54 | 70 | – | `submodule update` checks out gitlink → **gitlink apply (submodule cluster)** |
| t7506-status-submodule | 28 | 12 | 40 | **28** | status of submodule worktree → **partial gitlink + status (non-engine)** |

**Floor-enrolled in the candidate set (waves MUST NOT regress these):**
`t2020-checkout-detach = 16`, `t7102-reset = 37`, `t7400-submodule-basic = 70`,
`t7506-status-submodule = 28`. All four measured **at floor** on this branch
(16/37/70/28) — the engine work must hold them. (Adjacent non-engine floors in
the same numeric range — t1006=290, t1007=40, t3070=1861, t2107=10, t3600=50,
t3000=15 — are unaffected by unpack-trees changes.)

### Engine-attributable vs orthogonal (so the stage estimates are honest)

Of the 745 failing cells, a meaningful slice is **not** unpack-trees engine
work and should be discounted from yield estimates:

- **Rename detection** (t6426 ~11) — lives in `sley-diff-merge`, not unpack-trees.
- **Sparse-index serialization** (t1092 = 106, much of t1091) — the on-disk
  sparse-index format + expansion is a separate concern from the engine's
  sparse-merge arms.
- **Ref dwim / orphan / detach-advice porcelain** (large parts of t2024, t2060,
  t2020, t7103) — branch resolution and advice text, not tree application.

Netting those out, the **engine-attributable** opportunity is roughly **520–560
cells**, dominated by the submodule cluster (277) and the read-tree stat-info /
apply cluster (~110).

---

## 4. Stage table

Ordered by (yield ÷ effort) and dependency. The classic A→B→C→D shape holds, but
the data reorders the early stages: **stat-info + check_updates (Stage A) must
come before porcelain routing (Stage B)**, because routing checkout/switch onto
an engine that can't refresh stat-info would regress the dirtiness-sensitive
cells the hand-rolled path currently squeaks through.

| Stage | Engine work (functions / files) | Target t-files | Est. engine cells | Parallel? | Effort |
|---|---|---|---|---|---|
| **A — stat-info + check_updates apply** | Add `lstat` fields to `CacheEntry` (or a parallel stat map) and write them back on apply; make `check_updates` (`lib.rs:849`) re-stat written files, handle removals-before-writes ordering, symlinks, and **D/F directory replacement** (`verify_clean_subdirectory` port, currently TODO at `lib.rs:411,475`). Wire the `ReadTreeWorktree` probe (`read_tree.rs:446`) to populate stat-info. | t1000, t1001, t1002, t1004, t1005, t1008, t1012, t2000, t2003, t2006, t2007, t2008, t7104, t7113, t6400, t6408, t6424 | **~110–130** | **Sequential (foundation for B/D)** | **xhigh** (touches the CacheEntry model + index serialization; format-sensitive) |
| **B — route checkout/switch/restore/reset porcelain through the engine** | Delete `checkout_twoway_dirty` (`workspace.rs:1303`) and the ad-hoc reset apply; build a `WorktreeProbe`/`WorktreeWriter` for the real worktree (lift the FS impl out of `read_tree.rs` into a shared module); dispatch `cmd_checkout`/`cmd_switch`/`cmd_restore` via `twoway_merge`/`threeway_merge`, `cmd_reset` via `oneway_merge`; unify the porcelain error catalog with git's `setup_unpack_trees_porcelain` strings. | t2021, t2022, t2023, t2025, t2070, t2060, t7110 (reset-mode matrix), residual t7102, t6408, t6424 | **~70–90** | **Sequential (needs A)** | **xhigh** (large `workspace.rs` refactor; deletes a divergent fork — high blast radius, exact-text error parity) |
| **C — submodule gitlink checkout on the engine** | With A+B in place, the `is_gitlink` arm of `merged_entry` (`lib.rs:650,668`) + `check_submodule_move_head` (`sley-submodule/src/move_head.rs:125`) fire end-to-end: actually check out / update / move the submodule worktree during `check_updates` for gitlink entries, honor `--recurse-submodules`, `verify_clean_submodule`. | **t1013 (68), t2013 (73), t7112 (82), t7406 (54)**, plus t7400 / t7506 residual | **~250–280 (the big cluster)** | **Sequential (needs A+B apply path)** | **xhigh** (cross-crate: cli + sley-submodule; recursive worktree mutation) |
| **D — sparse / skip-worktree merge arms** | Implement the `S_ISSPARSEDIR`/`apply_sparse_checkout`/`mark_new_skip_worktree` arms (the `lib.rs:411,513,523,590` TODOs); add a `skip_worktree` bit to `CacheEntry`; the sparse *engine* half of read-tree / checkout under sparse patterns. (The sparse-index serialization for t1092 is a **separate** non-engine workstream — flag, don't fold in.) | t1011, t3602, t7002, engine-half of t1091 | **~50–70 (engine half)** | **Parallelizable with C** (disjoint files: sparse arms vs gitlink apply) | **medium** (bounded to the sparse merge arms once A lands) |

### Which stage unlocks submodule

**Stage C** is the one that converts the 277-cell submodule cluster from 0→
passing. It is **strictly gated on Stage A** (the gitlink checkout happens inside
`check_updates`, which Stage A makes capable of real worktree apply with
stat-info) and benefits from **Stage B** (so `checkout`/`reset` porcelain drive
the gitlink path, not just `read-tree`). This is the concrete confirmation of the
earlier finding: gitlink-apply lands 0 cells *until* unpack-trees' apply path is
complete — C cannot precede A.

### Parallelism / sequencing

- A → B → C is a strict chain (each needs the prior's apply capability).
- **D can run concurrently with C** once A lands — the sparse merge arms touch
  `lib.rs` sparse sites + the read-tree sparse path, disjoint from C's
  cli/sley-submodule gitlink files. Dispatch them as a 2-wide wave after A+B.
- Sequencing recommendation: **A (xhigh) → B (xhigh) → {C (xhigh) ∥ D
  (medium)}**. Re-measure and raise the four enrolled floors after each stage.

### Floor discipline reminder

After each stage, re-run the candidate set and **raise** the four enrolled
floors (t2020=16, t7102=37, t7400=70, t7506=28) to the new measured values, per
the durability discipline — a floor that lags the scoreboard lets a later wave
hide a regression in the slack.
