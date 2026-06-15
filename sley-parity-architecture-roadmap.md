# sley → git parity: architecture improvement roadmap

**Rev:** `faea0b0` (sley main, post #100) · **Oracle:** git 2.54.0 (PCRE) · **Generated:** 2026-06-15
**Baseline:** 62% in-scope parity — 16,143 pass / 9,958 fail across 746 in-scope test files (excludes t5xxx transport/servers, t9xxx foreign-SCM).

This roadmap was produced by six parallel analysis agents, one per engine cluster. Each read the per-cell failure data, the actual sley crate source, and git's C reference, then proposed the **strongest architectural fix** (a new engine, a shared primitive, an enforced invariant, a structural refactor) that closes a *class* of failures — not per-cell patches. Cell estimates are grounded in named t-files; scope is classified `real-correctness` vs `out-of-scope` (servers/foreign-SCM) vs `harness-gap` (interactive/external-tool flows we deliberately won't emulate).

---

## Executive summary

**The meta-finding: sley's parity gaps are overwhelmingly *missing or mis-architected engines*, not scattered bugs.** Across all six clusters the same shape recurs — a correct primitive exists but is *trapped* (private, unwired, or stubbed), or a whole subsystem git centralizes in one file (`ws.c`, `blame.c`, `convert.c`, `unpack-trees.c` gitlinks) was never built. That makes this an unusually *tractable* roadmap: the highest-leverage moves are **lift-to-primitive** refactors and single-engine builds that each close 100–270 cells at once.

The clearest examples:
- **The bisection algorithm is already correct** — it's just locked inside `bisect.rs`; lifting it to `sley-rev` + wiring `rev-list --bisect` closes ~52 cells with near-zero risk.
- **auto-crlf's 319 fails are not a missing convert engine** — the EOL math is right; it's a missing `core.safecrlf` warning *emitter* (~145) plus a smudge config-precedence *bug* (~230).
- **Submodules**: sley has the verification half (move-head probe) but not the worktree-apply half; one gitlink-apply primitive wired into unpack-trees closes ~270 cells across the 12 scripts that share `lib-submodule-update.sh`.
- **Blame**'s entire ~183-cell gap collapses to *one* root cause: a first-parent line-router instead of git's diff-driven scoreboard.
- **`git apply`** is ~25% complete because there's **no whitespace-rule engine** — a ~370-line `ws.c` port unblocks ~135 cells *and* `apply --whitespace=fix`.

**If the Phase 1 + Phase 2 work below lands (~1,450 real-correctness cells), in-scope parity moves from 62% → ~67%.** The full roadmap (~2,800 real cells, excluding harness/out-of-scope) targets **~72–75%**.

---

## Ranked roadmap (real-correctness opportunities)

Sorted by leverage = cells closed ÷ effort, weighted toward low risk and architectural soundness.

| # | Opportunity | Cluster | Cells | Effort | Risk | Depends on |
|---|---|---|---|---|---|---|
| 1 | **Submodule gitlink worktree-apply primitive** (one decision table wired into unpack-trees) | submodule | ~270 | L | Med | — |
| 2 | **Smudge config-precedence fix** (`output_eol` decision table) + `ls-files --eol` | convert | ~230 | M | Med | — |
| 3 | **Blame scoreboard** (diff-driven multi-pass, replaces first-parent router) | history | ~183 | L | Med | — |
| 4 | **`core.safecrlf` warning emitter** (additive on correct conversion) | convert | ~145 | M | Low | — |
| 5 | **Whitespace-rule engine** (`ws.c` port: `diff --check` + `apply --whitespace`) | diff | ~135 | M | Low | — |
| 6 | **`update-ref --stdin` command tokenizer** (quoted/escaped lexer + 4 die-messages) | refs | ~114 | M | Low | — |
| 7 | **Stash apply/pop rebuilt on the merge engine** (stop refusing dirty trees) | index | ~110 | L | Med | — |
| 8 | **Sparse/ignored explicit-path gate** (`path_in_sparse_checkout` shared primitive) | convert | ~110 | L | Med | — |
| 9 | **Diff hunk post-processor framework** (indent-heuristic, `-W`, inter-hunk, `-I`/pickaxe) | diff | ~115 | L | Med | userdiff (have) |
| 10 | **check-ignore / check-attr / sparse-checkout command-surface** completion | convert | ~100 | L | Low | — |
| 11 | **Recursing submodule move-head mutation engine** | submodule | ~85 | M | Med | #1 |
| 12 | **History-simplification engine** (ancestry-path, simplify-merges, follow, decoration) | history | ~80 | L | Med | TREESAME (have) |
| 13 | **Submodule diff renderer** (`--submodule=log/short` + dirty/untracked) | submodule | ~80 | M | Low | — |
| 14 | **format-patch series tooling + a range-diff engine** (`--base`/`--interdiff`/`--range-diff`) | diff | ~80 | L | Med | patch-id (have) |
| 15 | **Rebase `--no-ff` reflog discipline** (single root cause, 75-cell matrix) | index | ~75 | M | Med | — |
| 16 | **Line-log engine** (`git log -L`) — co-design chunk primitive with #3 | history | ~70 | L | Med | #3 |
| 17 | **worktree-add depth** (`--orphan` DWIM, bare-repo add + tracking) | index | ~67 | M | Low | — |
| 18 | **for-each-ref atom layer** (describe/align/if-then-else/color/version-sort) | refs | ~60 | L | Med | describe |
| 19 | **Combined-diff engine** (`-c`/`--cc`/`-m`) | diff | ~60 | L | Med | — |
| 20 | **`git submodule` porcelain depth** (relative-URL resolver + update-strategy) | submodule | ~55 | L | Low | — |
| 21 | **Lift bisection to `sley-rev` + wire `rev-list --bisect`** (algorithm already correct) | history | ~52 | M | Low | — |
| 22 | **commit-porcelain rendering** (oneline summary, cleanup modes, `--trailer`) | index | ~50 | M | Low | — |
| 23 | **Index UNTR/FSMN extensions + racy-clean write-back** | index | ~50 | L | Med | — |
| 24 | **Reachability primitive library** (`commit-reach.c` parity, ahead-behind/contains) | history | ~40 | M | Med | — |
| 25 | **`git replay` plumbing command** (ref-update-plan to stdout) | index | ~36 | M | Low | — |
| 26 | **Reflog policy in the transaction + `@{date}` resolution** | refs | ~34 | M | Med | — |
| 27 | **Transaction-wide D/F conflict + indirect-precondition checks** | refs | ~28 | M | Med | — |
| 28 | **`diff --max-depth`** tree-walk option | diff | ~46 | M | Low | — |
| 29 | **interpret-trailers config/placement completeness** | diff | ~35 | M | Low | — |
| 30 | **reference-transaction hook through the transaction lifecycle** | refs | ~9 | S | Low | — |
| — | **`git apply` → full `sley-apply` engine** (`apply.c` port) | diff | ~200 | XL | Med | #5 |
| — | **Real reftable backend** (log blocks, compaction, worktree stacks) | refs | ~32 | XL | High | — |

---

## Recommended build sequence

Dependency- and risk-ordered. Each phase is independently shippable and floor-gated.

### Phase 1 — Emitters & lift-to-primitive (low risk, high cells, fast momentum) — ~485 cells
Additive or front-end-only changes over already-correct cores; minimal blast radius.
- **#4 safecrlf emitter** (145) — additive stderr on a correct conversion engine.
- **#5 whitespace-rule engine** (135) — new self-contained `ws.c`-shaped module; also a hard dependency for the apply engine.
- **#6 update-ref --stdin tokenizer** (114) — front-end lexer swap; the transaction layer is already correct.
- **#15 rebase --no-ff discipline** (75) — one fix in the FF decision closes the whole 75-cell matrix.
- **#21 lift bisection** (52, minus overlap) — extract the proven algorithm to `sley-rev`, wire `rev-list --bisect`.

### Phase 2 — The big engines (medium risk, highest absolute leverage) — ~903 cells
The structural builds that each unlock a class.
- **#1 submodule gitlink worktree-apply** (270) — one primitive, 12 scripts.
- **#2 smudge config-precedence** (230) — fix `output_eol` precedence; conformance-grid the attr×autocrlf×eol×content matrix.
- **#3 blame scoreboard** (183) — diff-driven multi-pass; self-contained.
- **#7 stash on merge engine** (110) — route apply/pop through `merge_trees`/`threeway_merge`.
- **#8 sparse/ignored path-gate** (110) — one `path_in_sparse_checkout` primitive wired through add/rm/mv/reset.

### Phase 3 — Completeness layers (the post-processor & atom work) — ~700 cells
- #9 diff post-processor framework (115), #12 history-simplification (80), #16 line-log (70, co-designed with #3), #13 submodule diff renderer (80), #11 recursing submodule (85), #18 for-each-ref atoms (60), #19 combined-diff (60), #14 format-patch/range-diff (80).

### Phase 4 — Big bets & remaining depth
- **`git apply` → `sley-apply` crate** (~200, XL) — the largest single bucket; scope as its own crate *after* #5 lands.
- #10 command-surface completion (100), #17 worktree-add (67), #20 submodule porcelain (55), #22 commit-porcelain (50), #23 UNTR/FSMN (50), #24 reachability lib (40), #25 git replay (36), #26 reflog policy (34), #27 transaction D/F (28), #28 diff --max-depth (46), #29 trailers (35), #30 txn hook (9), reftable backend (XL, defer).

**Parity trajectory:** Phase 1 → ~64% · +Phase 2 → ~67% · +Phase 3 → ~70% · +Phase 4 (excl. reftable/apply-XL tail) → ~73–75%.

---

## Cross-cutting architectural themes

1. **Lift trapped primitives to shared crates.** The bisection finder (in `bisect.rs`), the `merge_bases_many`/`reduce_heads` reachability (re-implemented in `merge_rebase.rs`), and the submodule checkout (`reset_index_and_worktree_to_commit`, used only by `submodule update`) are all *correct but private*. Promoting them to `sley-rev`/`sley-submodule` closes cells *and* deduplicates. This matches the maintainer's standing "lift invariants to the primitive" preference.
2. **One decision table per transition class.** The submodule gitlink apply, the EOL `output_eol` resolution, and the whitespace-rule check are all cases where git has a *single* correct-by-construction function and sley has scattered per-command logic. Building the one table closes the whole class and prevents drift.
3. **Co-design line-range tracking once.** Blame (#3) and line-log (#16) both track line ranges backward through per-parent diffs. Build the chunk-tracking primitive once and both engines consume it.
4. **The whitespace engine is a keystone.** It independently closes ~135 cells *and* is the dependency for `apply --whitespace=fix` (part of the ~200-cell apply engine). Build it early.
5. **Floor-gate every phase.** These are shared-engine changes (ContentFilterPlan, unpack-trees, the ref transaction, the rebase FF decision all have wide blast radius). The 69-file floor gate must run on each PR, and floors raised as gains land.

---

# Detailed cluster analysis

The six per-cluster sections follow, each with root-cause citations (`crates/sley-*/src/...:function`), the proposed architecture, grounded cell estimates, and a flagged out-of-scope/harness list.

---


---

## Submodule subsystem
Cross-cutting cluster spanning ~830 failing cells in submodule-named scripts plus ~835 submodule-described cells leaking into many other t-files. The single dominant structural fact: **12 scripts source one shared test library (`lib-submodule-update.sh`) = 357 failing cells**, and they split cleanly into two engines — a **non-recursing gitlink worktree-apply** path (the big class, in all 12) and a **recursing submodule-mutation** path (only read-tree/checkout/reset). sley already has the verification half (`sley-submodule::move_head`, the unpack-trees `WorktreeProbe::check_submodule_move_head` hook) and a working full-tree mutation primitive (`sley_worktree::reset_index_and_worktree_to_commit`); the gap is that neither is wired into the unpack-trees worktree-apply seam. Diff and porcelain are separate, smaller clusters.

### Submodule-aware unpack-trees worktree-apply (non-recursing gitlink engine) — leverage: HIGH
- **Root cause:** The unpack-trees engine (`crates/sley-unpack-trees/src/lib.rs:is_gitlink`, `merged_entry`) correctly computes the merged *index*, and the CLI probes (`read_tree.rs:ReadTreeWorktree::check_submodule_move_head`, the checkout/reset equivalents) call the *verification* logic. But the **worktree-apply side** for gitlinks is missing/incomplete in the unpack-trees-driven path. The non-recursing harness (`lib-submodule-update.sh:test_submodule_switch_common`) asserts a precise directory-management contract that lives in git's `unpack-trees.c`/`entry.c` gitlink branches: (a) an *appearing* submodule must create an **empty directory** at the gitlink path (and tolerate a pre-existing empty dir, and replace a tracked file/dir with an empty dir); (b) a *disappearing* submodule must **leave its worktree directory and contents in place** (git never auto-removes a populated submodule on a non-recursing switch); (c) D/F-conflict refusals (replace-submodule-with-file/dir-containing-.git must fail). sley's full-tree restore (`sley-worktree/src/lib.rs:6061` → `materialize_tree_entry:6676`) already does the empty-dir mkdir, but the unpack-trees checkout/reset/read-tree path doesn't route appearing/disappearing gitlinks through the same correct-by-construction apply, and `checkout_remove_gitlink_worktree_dir` (`workspace.rs:1469`) only `remove_dir`s an empty dir rather than implementing the leave-in-place rule.
- **Architecture:** Add a **gitlink worktree-apply primitive** to `sley-worktree` (or a new `sley-submodule::apply` module) with one decision table over (old_mode, new_mode) for gitlink transitions — `appear → mkdir empty + skip-if-exists-empty`, `disappear → no-op (leave in place)`, `gitlink↔file/dir → D/F verify_clean refusal`. Have the unpack-trees CLI consumers (`read_tree.rs`, `workspace.rs::cmd_checkout`/`cmd_reset`) drive their worktree-apply phase through this single primitive instead of ad-hoc per-command code. This makes the whole non-recursing class pass identically across read-tree/checkout/reset/rebase/am/apply/cherry-pick/revert/stash/bisect, because all of them flow their tree-switch through the same engine (that is exactly why git gets all 12 for free from one code path).
- **Closes:** ~270 cells — the non-recursing subset of t1013(54)/t2013(54)/t7112(66) + all of t6438(48), t3426(25), t4137(24), t4255(24), t3512(12), t3513(12), t3906(4), t6041(12). (The 12-script lib total is 357; ~270 are non-recursing, the rest are the recursing opportunity below.)
- **Effort:** L · **Risk:** Medium — shared unpack-trees/worktree blast radius; the leave-in-place vs remove asymmetry is subtle and must not regress ordinary directory removal.
- **Depends on:** none (verification hooks + `materialize_tree_entry` already exist; this wires + extends them).
- **Scope:** real-correctness

### Recursing submodule move-head mutation engine — leverage: MEDIUM
- **Root cause:** `sley-submodule/src/move_head.rs` is explicitly **only the dry-run verification path** — its module doc and `MoveHeadFlags` comment defer the non-dry-run mutation (`submodule_move_head` proper: absorb gitdir, `read-tree -u` inside the submodule, update submodule HEAD) as `TODO(submodule)` to the caller. So `git checkout/read-tree -u -m/reset --keep --recurse-submodules` parse the flag (`workspace.rs:58,584` `--no-recurse-submodules => {}`) but never recurse into the submodule worktree. The recursing harness (`test_submodule_switch_recursing_with_args`) asserts `test_submodule_content sub1 origin/<rev>` — the submodule's *working tree* must actually be checked out to the new gitlink commit, nested submodules included.
- **Architecture:** Implement the mutation half of `submodule_move_head` as a `sley-submodule` function that the unpack-trees apply phase calls when `--recurse-submodules` is set. The checkout primitive **already exists and is proven** — `cmd_submodule_update` (`submodule.rs:416`) calls `sley_worktree::reset_index_and_worktree_to_commit` to detach-checkout a submodule to a target oid. Lift that into the shared engine (absorb-gitdir + reset-index-and-worktree + write submodule HEAD + recurse), and invoke it from the move-head probe's mutation arm instead of only verifying. Reuse the typed `SubmoduleConfigSet` + `is_submodule_active` already resolved in the read-tree probe.
- **Closes:** ~85 cells — the recursing subset of t1013, t2013, t7112 (the `--recurse-submodules` / `-c submodule.recurse=true` halves), ~28 each.
- **Effort:** M · **Risk:** Medium — nested recursion + absorb-gitdir edge cases; force vs non-force interaction with the existing verification gate.
- **Depends on:** the verification engine (already shipped) + ideally the non-recursing apply primitive above (shared mkdir/remove rules).
- **Scope:** real-correctness

### Submodule diff renderer (`--submodule=log` / `=short` + dirty/untracked) — leverage: MEDIUM
- **Root cause:** sley renders a changed gitlink only as the raw `Subproject commit <oid>[-dirty]` blob diff (`sley-cli/src/lib.rs:gitlink_diff_content:3480`). The `--submodule[=log|short|diff]` flag is parsed but discarded — `diff_options.rs:962` sets `diff_submodule_output_control = true` and does nothing with it, and there's no `Submodule <path> <sha1>..<sha2>:` log-block renderer nor the "contains untracked/modified content" annotations. git's default (`diff.submodule=log`) walks the submodule repo to print the commit-range log (`submodule.c:show_submodule_diff_summary`/`show_submodule_header`).
- **Architecture:** Add a submodule-diff rendering module to `sley-diff-merge` keyed off the existing gitlink helpers (`gitlink_git_dir`, `gitlink_head_oid`, `submodule_dirt` in `sley-worktree`). For a gitlink change, dispatch on the resolved format: `short` → keep current `Subproject commit`; `log` → open the submodule odb, compute the rev-range, emit the `Submodule <path> <old>..<new>:` header + first-line commit summaries + the `contains untracked/modified content` flags; honor `submodule.<name>.ignore`/`diff.ignoreSubmodules` (the `IgnoreSubmodules` resolver already exists in `workspace.rs`). The dirty/untracked detection reuses `sley_worktree::submodule_dirt`.
- **Closes:** ~80 cells across t4041(42), t4060(44) minus their ~15-each lib-coincidental cells → ~54 diff-format cells, plus t4059-diff-submodule-not-initialized(7), t7506 .gitmodules-conflict diff cells, and t4027(2).
- **Effort:** M · **Risk:** Low — additive rendering, isolated to the diff path; main subtlety is exact log/range formatting and ignore-mode precedence.
- **Depends on:** none.
- **Scope:** real-correctness

### `git submodule` porcelain depth (url-resolution, update-strategy, init/config) — leverage: MEDIUM
- **Root cause:** The porcelain is broadly present (`submodule.rs`: add/update/init/deinit/sync/foreach/summary/set-url/set-branch all stubbed in `cmd_submodule`), but shallow. t7400(54) failures cluster in **relative-URL resolution** (16 cells: `../subrepo`, scp-style, `file://`, `ssh://` — git's `relative_url()`/`resolve_relative_url` in `builtin/submodule--helper.c`) and init/add config plumbing (27 cells); t7406(54) clusters in **update strategies** (15 cells: `--rebase`/`--merge`/`update=none`/`.git/config` precedence) and recursion (8). These are per-feature depth gaps in one command, not a missing engine.
- **Architecture:** Two shared primitives in `sley-submodule`: (1) a `resolve_relative_url(superproject_remote, modules_url)` function porting git's relative-URL algorithm (closes the t7400 `../subrepo` block in one shot); (2) an update-strategy dispatcher honoring the `update=checkout|rebase|merge|none` precedence chain (CLI > `.git/config` > `.gitmodules`), reusing the existing `update_type_to_string`/`UpdateType` enum in `sley-submodule::config`. Wire `--recursive` through both. Foreach/sync/summary are mostly output-format fidelity on top.
- **Closes:** ~55 cells — t7400 url+init (~31), t7406 strategy+config (~24), with spillover to t7403-sync(16), t7419/t7420 set-branch/set-url, t7421 summary-add.
- **Effort:** L (breadth, many small behaviors) · **Risk:** Low-Medium — self-contained to the submodule command + new helpers.
- **Depends on:** none.
- **Scope:** real-correctness (the clone/fetch-dependent subset is out-of-scope — see below).

### Flagged out-of-scope / harness gaps
- **Network clone/fetch (out-of-scope):** t7406 `submodule update --remote should fetch upstream changes`, `clone shallow submodule`, `--depth`/`--reference` (~10 cells); t7400 fetch-dependent add cells (~5); t7408-submodule-reference(11, alternates/`--reference`), t5572-pull-submodule, t7403 remote-sync cells. These require submodule fetch/clone over a transport sley deliberately doesn't host. ~30+ cells.
- **`submodule foreach` shell harness (harness-gap):** t7407(19) runs arbitrary shell per submodule with env (`$name`/`$sm_path`/`$sha1`/`$toplevel`) — the failing cells need a faithful shell-invocation harness, not a sley engine. Recursive-foreach checkout of 2nd-level submodules overlaps the mutation engine.
- **`--recurse-submodules` recursion into subrepos for read commands (real-correctness but separate engine):** t7814-grep-recurse-submodules(23) and t3007-ls-files-recurse-submodules(22) parse `--recurse-submodules` but ignore it (`grep.rs:389`, `index.rs:502` ignore-lists). These need a generic "open the submodule repo and re-run the read command with a path prefix" recursion harness — shared between grep/ls-files but distinct from the worktree-mutation engine. ~45 cells; worth a dedicated recursion primitive if prioritized, but not a submodule-engine gap per se.
- **`update --rebase/--merge` running real rebase/merge inside the submodule (depends on those engines):** the t7406 strategy cells that actually invoke rebase/merge depend on sley's rebase/merge engines being driven inside the subrepo — partially blocked on those clusters, not pure submodule work.
- **`# TODO known breakage` cells:** several `replace submodule with a directory`/`...with a file must fail` cells across t1013/t2013/t7112/t6438 are marked known-breakage *in upstream git itself* — sley matching git here means leaving them failing; don't count them as closeable correctness wins (~8-10 cells).

---

## Convert, attributes, ignore & sparse-checkout
**Files:** t0027-auto-crlf (319), t0028-working-tree-encoding (21), t0020/t0021/t0022/t0025/t0026 (~50), t0003-attributes (27), t3920-crlf-messages (9), t2082 (5) · t0008-ignores (92), t2204-add-ignored (43), t7061-wtstatus-ignore (21), t3001 (10) · t1092-sparse-compat (106), t1091-sparse-builtin (55), t1011 (18), t7002-mv-sparse (19), t3705-add-sparse (17), t3602-rm-sparse (10), t7817-grep-sparse (6), t1090 (4), t6428/t6435 (4) · t6135-pathspec-attrs (32), t6132-pathspec-exclude (29). **Headline:** the EOL/attribute *math* is genuinely well-built (`ContentFilterPlan` mirrors `convert.c`), so the huge t0027 number is **not** a missing convert engine — it's two surgical class-gaps (a missing safecrlf warning emitter + a smudge-direction config-precedence bug) plus an unbuilt working-tree-encoding stage; the sparse number is a missing **sparse/ignore path-gate invariant** shared across add/rm/mv/restore plus the unbuilt sparse-index extension.

### Safecrlf round-trip warning emitter (the dominant t0027 class) — leverage: HIGH
- **Root cause:** sley's clean/smudge pipeline performs the byte conversion but emits **zero** of git's round-trip warnings. `crates/sley-worktree/src/lib.rs::apply_clean_filter_with_attributes_cow` (line 5393) and `::apply_smudge_filter_with_attributes_cow` (5447) convert silently; there is no analogue of git `convert.c::check_global_conv_flags_eol` (the `"LF will be replaced by CRLF in %s"` / `"CRLF will be replaced by LF in %s"` text) and no `core.safecrlf` plumbing. `grep "will be replaced by"` over `crates/` is empty. The t0027 `commit_check_warn`/`commit_chk_wrnNNO`/`commit_MIX_chkwrn` cells assert exactly this stderr.
- **Architecture:** add a `ConvFlags`/`SafeCrlf` parameter to the *clean* entry points and a `check_safe_crlf(old_stats, new_stats, conv_flags, path)` helper next to `gather_convert_stats` (which already exists, 5553). Compute `old_stats` (pre-conversion) and `new_stats` (post) — sley already has both buffers in the cow path — and emit the warning when `old.crlf && !new.crlf` (CRLF_LF) or `old.lonelf && !new.lonelf` (LF_CRLF), gated by `core.safecrlf=warn|true`. Thread `core.safecrlf`/`core.autocrlf=true`-implies-warn through `cmd_add`/commit's index-update call (`add_paths_to_index_filtered`, 739). This is a pure additive emitter on an existing, correct conversion — no risk to the byte output.
- **Closes:** ~130 in t0027 (`commit files`=70, `commit NNO`=50, `commit file with mixed`=15) + t0020 safecrlf (4) + **all 9** of t3920-crlf-messages + a few in t0025-renormalize (2). ~145 cells.
- **Effort:** M · **Risk:** Low — additive stderr; the conversion engine it instruments is already correct.
- **Depends on:** none.
- **Scope:** real-correctness

### Smudge config-precedence + `ls-files --eol` worktree-stat fix — leverage: HIGH
- **Root cause:** two linked bugs under `core.autocrlf=true core.eol=lf`. (1) **Checkout smudge** under-converts: 150 of 1092 `checkout` cells fail, concentrated on `file=LF` (60) and `LF_mix_CR`/`CRLF_mix_LF`/`LF_nul` — content with a naked LF that should gain CRLF. `ContentFilterPlan::resolve` (4940) derives `eol` from config via `eol_from_config` (5065) but the precedence where `core.autocrlf=true` must *override* `core.eol`/unset-eol (t0026: "autocrlf=true overrides eol=lf", "overrides unset eol") and the `text` (non-auto) vs `text=auto` smudge gating interact wrongly for these classes; `will_convert_lf_to_crlf`'s auto-safety guard (5045) is firing on non-auto `attr=text` paths it shouldn't, or `eol` resolves to `Lf` when `autocrlf=true` should force `Crlf`. (2) **`ls-files --eol`** `w/` field (74 fails, 46 of them `autocrlf=true`): `eol_info_for_path` (5733) reads the on-disk file and runs `convert_stats_ascii`, but the on-disk bytes are wrong precisely because of bug (1), so the `w/` stat is `lf` where git shows `crlf`.
- **Architecture:** make `EolConversion` resolution a single correct-by-construction function matching git's `output_eol(crlf_action)` decision table (`convert.c` lines ~150–195): autocrlf=true ⇒ EOL_CRLF wins over `core.eol`; `attr=text` (not auto) always converts naked LF on smudge (skip the auto-only safety guard); `attr=text=auto` keeps the guard. Fixing (1) auto-fixes (2) for free since `ls-files --eol` reads the materialized file. Add a focused conformance test over the t0027 attr×autocrlf×eol×content matrix so the whole grid is gated.
- **Closes:** ~150 checkout + 74 `ls-files --eol` in t0027 + t0026 (3) + t0020 round-trip cells ("LF only file gets CRLF", "New CRLF file gets LF") (~4) + t0021/t0025 spillover. ~230 cells.
- **Effort:** M · **Risk:** Medium — `ContentFilterPlan` is the shared clean+smudge+archive+hash-object engine; a precedence change has blast radius across every materialization surface (conformance grid mitigates).
- **Depends on:** none (composes with the warning emitter above — same `ContentFilterPlan`).
- **Scope:** real-correctness

### Sparse / ignored explicit-path gate — one shared `path_in_sparse_checkout` + add-ignore invariant — leverage: HIGH
- **Root cause:** commands don't treat the sparse cone or `.gitignore` as a **gate** on explicitly-named paths. `add.rs`/`rm.rs`/`mv.rs` have **zero** sparse/skip-worktree awareness (`grep skip_worktree` over them is empty); `cmd_add` (`plumbing.rs:845`) parses `--force`/`--sparse` but its body never (a) refuses an explicitly-named ignored path with `"The following paths are ignored by one of your .gitignore files"` + non-zero exit (git `builtin/add.c:232`), nor (b) refuses/skip a path outside the sparse cone with the `advice.updateSparsePath` hint (absent from the whole tree — `grep updateSparsePath` empty). Git centralizes this in `dir.c::path_in_sparse_checkout` / `path_in_cone_mode_sparse_checkout`, consumed by `pathspec.c:53,84`, `builtin/reset.c:182`, `builtin/{add,rm,mv}.c`, `attr.c:821`, `diff.c:4413`, `merge-ort.c:4663`.
- **Architecture:** build one shared primitive in `sley-worktree` — `path_in_sparse_checkout(path, index) -> bool` reusing the existing `SparseMatcher` (3736) over the loaded `.git/info/sparse-checkout` + `core.sparseCheckout` — and one `classify_explicit_pathspec()` seam that, for each user-named path, returns Tracked / Ignored / OutsideSparse, with the two advice strings. Wire it through `cmd_add` (refuse ignored unless `-f`; refuse/skip OutsideSparse unless `--sparse`), `cmd_rm`, `cmd_mv`, `reset`. Make it the single gate so the class is closed structurally (lift-to-primitive), not patched per command. The `attr.c:821` consumer (cone mode skips attr lookup for out-of-cone paths) also threads through here.
- **Closes:** t2204 (43, ignored-path gate) + t3705-add-sparse (17) + t3602-rm-sparse (10) + t7002-mv-sparse (19) + t1090 (4) + part of t1011 read-tree sparse honoring (~10) + t7817 grep sparse (6) + t6428/t6435 sparse-merge (4). ~110 cells across 8 files.
- **Effort:** L · **Risk:** Medium — touches add/rm/mv/reset write paths; the gate must not change behavior for in-cone/unignored paths (the 86% that pass today).
- **Depends on:** none — `SparseMatcher` already exists; this is wiring + advice strings.
- **Scope:** real-correctness

### `git check-ignore` / `git check-attr` / `sparse-checkout` command-surface completion — leverage: MEDIUM
- **Root cause:** the *matching* engines pass (t0008 ignore-match 77%, t0003 attr 50%); the failures are missing command-surface options. t0008 (92): `check-ignore --stdin`/`-z`/`-q`/`-v`/`--verbose`, error-arg validation ("erroneous use of", "empty command line", "--stdin with superfluous"), and "beyond a symlink" (18). t1091 (55): `git sparse-checkout` is missing the entire **`check-rules`** subcommand (7 cells — not in the `match sub` at `sparse_checkout.rs:61`) and non-cone `list (populated)` ordering. t7061-wtstatus-ignore (21): `git status --ignored[=mode]` directory-collapsing display.
- **Architecture:** these are option-parser/output-formatter completions on top of working engines, not new engines. Add `check-rules` to `cmd_sparse_checkout` (it just runs `SparseMatcher` over stdin paths). Add the `--stdin`/`-z`/`-q`/`--verbose` flag matrix + arg-validation to the existing `check-ignore` command. Wire `--ignored[=traditional|matching]` directory roll-up into `short_status_with_options` (1815). Lower-leverage but mechanically bounded.
- **Closes:** t0008 (~74, excluding the 18 symlink + submodule cells) + t1091 check-rules (7) + t7061 (~18) + t3001 ls-files-others-exclude (10). ~100 cells, lower density per fix.
- **Effort:** L (breadth, not depth) · **Risk:** Low.
- **Depends on:** none.
- **Scope:** real-correctness (the 18 "beyond a symlink" t0008 cells are a symlink-resolution edge of the ignore walker)

### Flagged out-of-scope / harness gaps
- **t1092-sparse-checkout-compatibility (106 cells, 0%) — partial out-of-scope.** Entire file is downstream of a failing `setup` (cell 1 dies `sley: command failed: missing command` — a setup command translates to a bare `sley` invocation). Even with setup fixed, the file is a `GIT_TEST_SPARSE_INDEX` matrix testing the **sparse-index extension** (tree-collapsed index format) — 22 cells literally titled `sparse-index is/not`; sley accepts `--sparse-index` flags but no-ops them (`sparse_checkout.rs:90,209`). The sparse-index extension is a large separate engine; treat as a deliberate feature gap, not a correctness gap. The non-sparse-index half could light up once the shared path-gate (opportunity 3) lands and the setup command is added.
- **t0028-working-tree-encoding (21 cells) — unbuilt engine, likely deprioritize.** UTF-16/UTF-32 ↔ UTF-8 `working-tree-encoding` attribute via iconv — git's `convert.c::encode_to_git`/`encode_to_worktree`. Not implemented in sley (the `working_tree_encoding` hits are unrelated log/cli refs). A self-contained re-encoding stage that bolts onto `ContentFilterPlan` before EOL; medium engine, low test count — lower priority than the EOL fixes.
- **t6135-pathspec-with-attrs (32) — different cluster (pathspec engine).** `:(attr:foo)` pathspec magic; consumes the attribute engine but the gap is in `sley-pathspec`, not the convert/attr engine. Flag for the pathspec cluster owner.
- **t6132-pathspec-exclude (29) — different cluster (pathspec engine).** `:(exclude)`/`:!` negative pathspec; belongs to `sley-pathspec`, not the ignore engine.
- **t7301-clean-interactive / t7300-clean** — clean's interactive prompt loop is a harness-gap if present; the non-interactive clean cells belong with the worktree-removal owner.

---

## Refs, reflog & reftable + transactions
Cluster scripts: t1400-update-ref (138), t0610-reftable-basics (60), t6300-for-each-ref (71), t6302-for-each-ref-filter (50), t1461-refs-list (70), t7004-tag (72), t3200-branch (50), t1404-update-ref-errors (24), t1430-bad-ref-name (25), t1410-reflog (27), t1411/t1413/t1414/t1416/t1417/t1421 (reflog+hooks), t6120-describe (38). **Headline:** sley already has a real unified `FileRefTransaction` (atomic loose-ref commit, lock-then-verify CAS, packed-refs rewrite) — the gaps are *around* it, not a missing transaction primitive. The four highest-leverage architectural holes are (1) the `update-ref --stdin` command-stream **tokenizer** (split_whitespace vs git's quoted/escape lexer — `crates/sley-cli/src/commands/refs.rs:1094`), (2) a missing **describe/align/if/color/version-sort** atom layer in the for-each-ref formatter, (3) a **degenerate reftable backend** (ref-blocks only, no log/compaction/worktree), and (4) **reflog @{date} resolution + a centralized reflog-write policy** that today is unimplemented and duplicated per-command. A very large share of nominal failures (~110 cells) are **gpg-signing harness gaps** that cascade, not correctness gaps.

### Replace the `update-ref --stdin` line splitter with git's command-stream tokenizer — leverage: HIGH
- **Root cause:** `update_ref_stdin_line` (`crates/sley-cli/src/commands/refs.rs:1094`) tokenizes each line with `line.split_whitespace()` and silently `Ok(())`s empty lines (`refs.rs:1073`). git's `builtin/update-ref.c` uses a stateful lexer (`parse_next_arg`, lines 30-45; `parse_cmd_*`) that distinguishes `empty command in input`, `whitespace before command:`, `badly quoted argument:`, `unexpected character after quoted argument:`, and supports `"…"`-quoted args with `\`-escapes and embedded spaces/NULs. sley emits a single `fatal: unknown command:` (`refs.rs:1811`) for all of these, so every error-shape and every quoted/escaped-arg cell mismatches; whitespace-prefixed and empty lines are mis-accepted.
- **Architecture:** add one shared `RefCommandStream` lexer in `sley-cli` (or `sley-refs`) modeling git's `struct strbuf input` reader: parse `<command> SP <args…>` for the `\n` path and `<command> NUL <arg> NUL …` for `-z`, with the four distinct die-messages and a quoted-arg decoder. Both `update_ref_stdin` and `update_ref_stdin_z` (`refs.rs:1063`/`1065`) consume it, then feed the existing `FileRefTransaction` — the transaction layer is already correct, so this is purely a front-end lexer swap. Carry `update`/`create`/`delete`/`verify`/`option`/`symref-*`/`start`/`prepare`/`commit`/`abort` through one dispatch table.
- **Closes:** ~114 cells — t1400 ~90 (cells 74-160: stdin/-z parsing, all symref-create/update/delete/verify, batch-updates), t1404 (stdin bad-name cases), t1430 ~6 (`--stdin` bad-ref-name).
- **Effort:** M · **Risk:** low — front-end only; the atomic commit path is untouched and already tested.
- **Depends on:** none.
- **Scope:** real-correctness.

### Add the describe / align / if-then-else / color / version-sort atom layer to the for-each-ref formatter — leverage: HIGH
- **Root cause:** `sley-ref-filter` + `for_each_ref.rs` implement most atoms but **entirely lack** `describe`/`describe:tags|abbrev|match|exclude` (0 hits), `align:`/padding magic (0 hits), `%(if)…%(then)…%(else)…%(end)` (0 hits), `is-base` (0 hits), `%(color:…)` (2 hits, partial), version-sort, and `contents:lines=N`. git centralizes these in `ref-filter.c` (`grab_describe_values:1975`, `grab_sub_body_contents:2013`, align/if are state atoms in the format machinery). sley's gaps make the t1461/t6300 "describe atom", "align:*", "%(if)*", "version sort", "contents:lines" cells fail identically across both files (the two scripts share the same atom matrix).
- **Architecture:** extend the existing shared `ForEachRefFormat` segment model in `crates/sley-ref-filter/src/lib.rs` with (a) state atoms — `align`/`end` and `if`/`then`/`else`/`end` as a small bracket stack evaluated at render time (git's approach), (b) a `describe` atom that calls the existing describe engine, (c) a `version:refname`-style version comparator for `--sort=version:*`/`v:*`, and (d) `contents:lines=N` slicing in `for_each_ref_message_parts` (`ref-filter/lib.rs:589`). Keep it in `sley-ref-filter` so `for-each-ref`, `tag --format`, and `branch --format` all inherit it (today each command re-derives atoms).
- **Closes:** ~60 real-correctness cells — t6300 ~12 (describe×6, raw/if, custom-date sort), t1461 ~12 (mirror set), t6302 ~30 once its setup passes (align×15, if/then/else×6, version-sort×3, contents:lines×3, color). (Excludes the ~22 signed-* atom cells per file, which are the gpg cascade below.)
- **Effort:** L · **Risk:** medium — align/if are stateful and the format machinery is shared by 3 commands; describe atom pulls in the describe engine.
- **Depends on:** describe engine (t6120 cluster) for the `describe` atom subset; the rest is independent.
- **Scope:** real-correctness.

### Make the reftable backend real — log blocks, auto-compaction, worktree stacks, proper footer — leverage: MEDIUM
- **Root cause:** the reftable backend is minimal: `append_reftable_records` (`crates/sley-refs/src/lib.rs:1313`) writes **one table per transaction** and never compacts; `sley_formats::Reftable` exposes only `write_ref_only` with a footer hardcoded to `(0,0,0,0,0)` (`crates/sley-formats/src/lib.rs:155,214`) — there is **no `LogRecord`/log-block type at all**, so reftable repos cannot store reflogs; there is no per-worktree `worktrees/<id>/reftable` stack and no `tables.list` lock/compaction protocol. This fails every t0610 compaction, reflog, worktree, and "writes are synced" cell.
- **Architecture:** three additions, in order of leverage: (1) a reftable **log block** writer/reader in `sley-formats` (git's `reftable/block.c` log records) so reflog append/iterate route through the same store the loose backend uses; (2) an **auto-compaction** policy in `append_reftable_records` driven by `tables.list` (geometric table-size sequence + `tables.list.lock`, matching git's `reftable/stack.c`) with the `GIT_TEST_REFTABLE_…` env disable hook; (3) **worktree stack** routing (`ref_base_dir`-equivalent for reftable: shared stack in `common_dir/reftable`, per-worktree stack in `worktrees/<id>/reftable`). The loose↔reftable split should sit behind one `RefBackend` trait the transaction already branches on (`commit` at `refs/lib.rs:1683`), so the transaction semantics stay shared.
- **Closes:** ~32 cells in t0610 (reflog×9, compaction/auto-compaction×11, worktree×9, sync×3); does not touch `--shared` umask (separate fs-permission gap, ~13 cells).
- **Effort:** XL · **Risk:** high — new on-disk format surface (log blocks, index/obj footer fields) + concurrency (table-list locking); blast radius is every reftable repo.
- **Depends on:** none (loose backend is the fallback), but the reflog log-block work pairs with the reflog-policy opportunity below.
- **Scope:** real-correctness.

### Centralize reflog write-policy + add @{date} resolution (one reflog engine, not per-command) — leverage: MEDIUM
- **Root cause:** two separate holes. (a) **@{date} is unimplemented:** `resolve_at_brace` returns `GitError::Unsupported` for any non-numeric, non-`u/push` selector (`crates/sley-rev/src/lib.rs:474-478`), so every `main@{2005-05-26 …}` / `@{now}` / `--date` reflog-time query fails (t1400 #46-60, t1411). (b) **Reflog-write policy is duplicated and caller-driven:** `FileRefTransaction` takes an `Option<ReflogEntry>` the caller must pre-build, and the "should we log?" decision (`update_ref_should_write_reflog`, `refs.rs:2298`, honoring `core.logAllRefUpdates=true|always|false` and the `refs/heads|refs/remotes` default) lives only in update-ref; branch.rs/tag.rs re-parse `--create-reflog` independently (`branch.rs:6090`, `tag.rs:91`). So HEAD-reflog-on-delete, `logAllRefUpdates=always` (tags/HEAD), and reflog-on-create defaults diverge per command.
- **Architecture:** (1) a reflog **date-index resolver** in `sley-rev` that parses `@{<approxidate>}` and binary-searches a ref's reflog by committer-timestamp, emitting git's `warning: log for '<ref>' only goes back to …` boundary message (git's `read_ref_at` in `refs.c`); reuse the date parser already used elsewhere in sley. (2) Lift the log-or-not decision **into the transaction primitive**: `FileRefTransaction::commit` consults a `ReflogPolicy` (config + ref-class + `--create-reflog` override) and synthesizes the `ReflogEntry` from the queued `(old,new,message)` itself, so update-ref/branch/tag/checkout stop hand-rolling it. This also fixes "update-ref should also create reflog for HEAD" and the t0610 "updates via HEAD update HEAD reflog".
- **Closes:** ~34 cells — reflog @{date} ~26 (t1400 #46-60, t1411), reflog-policy ~8 (t1400 HEAD-log, t3200/t7004 `logAllRefUpdates`/`--create-reflog`, t0610 HEAD-reflog).
- **Effort:** M · **Risk:** medium — moving reflog synthesis into the transaction is a shared-engine change touching all ref-mutating commands.
- **Depends on:** pairs with the reftable backend (reflog log-blocks) for reftable repos.
- **Scope:** real-correctness.

### Transaction-wide D/F conflict + indirect-precondition checking — leverage: MEDIUM (smaller, high-confidence)
- **Root cause:** `check_ref_directory_conflict` (`crates/sley-refs/src/lib.rs:1500`) checks each created ref against **on-disk** state (`read_ref_unchecked` + `list_refs`), and is only invoked for `Update` changes in `commit_loose` (`refs.rs:1758`). git computes D/F conflicts over the **whole transaction set** (creates ∪ deletes), so "add `foo/bar` + delete `foo`" and the symref-indirect variants are rejected as a batch. sley's per-ref, disk-only check plus its handling of *indirect* (through-symref) precondition verification miss the t1404 "D/F conflict prevents …" and "indirect … blocks …" families.
- **Architecture:** before staging, build the transaction's resulting name-set = (existing refs − queued deletes) ∪ queued creates, and run one D/F pass over that set (any name that is a strict path-prefix of another conflicts); resolve symref-indirect updates to their leaf before precondition checks so old-oid verification matches git's `lock_ref_for_update` indirection. This is a refinement of the existing transaction commit, not a new subsystem.
- **Closes:** ~28 cells — t1404 ~24 (all D/F + indirect-precondition), plus the t1416 "F/D conflict" and a couple t1430 cases.
- **Effort:** M · **Risk:** medium — changes the commit-time validation order; must not regress the already-passing single-ref cases.
- **Depends on:** none.
- **Scope:** real-correctness.

### Wire the reference-transaction hook through the transaction lifecycle — leverage: LOW
- **Root cause:** sley fires `reference-transaction` only ad-hoc from `workspace.rs` (commit path, `crates/sley-cli/src/commands/workspace.rs:850-987`), not from `FileRefTransaction::commit`. t1416 needs the hook called in **prepared / committed / aborted** phases with the full queued update list (incl. symref updates) on stdin.
- **Architecture:** invoke the hook from inside `FileRefTransaction::commit` (`refs/lib.rs:1680`) at the three lifecycle points, feeding the coalesced change set; since the transaction already owns the queued changes this is the natural home (matches git's `ref_transaction_prepare/commit` hook calls).
- **Closes:** ~9 cells (t1416).
- **Effort:** S · **Risk:** low. · **Depends on:** none. · **Scope:** real-correctness.

### Flagged out-of-scope / harness gaps
- **GPG / SSH tag signing + signature atoms (~110 cells, HARNESS+FEATURE):** `git tag -s` returns `"signed tag creation is not implemented"` (`crates/sley-cli/src/commands/tag.rs:403-407`). This cascades **all** `signed-empty/short/long` atom cells in t1461 (#293→#313) and t6300 (mirror set), plus ~30 t7004 sign/verify cells and the t1461/t6300 `signature`/`contents:signature` atom cells (which need real signatures to grade). Needs a gpg/ssh signing backend (a deliberate feature + external-tool harness), not a formatter fix — do not count toward the formatter opportunity.
- **Editor-driven flows (~16 cells, HARNESS):** t7004 `-m … --edit`, `--trailer`-forces-editor, `--edit-description`; t3200 `--edit-description` — all need an editor harness; flag, don't emulate.
- **`--column` output (~8 cells, FEATURE):** t3200/t7004 `--column`/`column.ui` — column-formatter feature, low priority, unrelated to the ref store.
- **`--shared=umask|group|world` permissions (~13 cells in t0610, FEATURE):** init/pack-refs honoring shared-repo file modes — a filesystem-permission feature, separate from the reftable engine.
- **reflog expire/walk engine (t1410 ~25, t1414 ~9):** `git reflog expire` (gc/expiry policy, `--stale-fix`, multi-worktree) and `git log -g` reflog-walk with pathspec/parents/date-limiting are their own engine (revwalk over reflog), distinct from the ref-store; real-correctness but a separate cluster from the transaction theme.
- **describe / name-rev engine (t6120 ~38):** the `git describe`/`name-rev` algorithm is its own engine; only its `describe` *atom* surface overlaps this cluster (counted once under the formatter opportunity).
- **Worktree-aware branch rename (t3200 `-M` to linked worktree, ~6):** depends on the worktree-ref-store routing (overlaps the reftable worktree-stack work for reftable repos; loose-backend variant is a separate worktree-HEAD-update concern).

---

## Revision walking, history & blame

Two engines under one cluster. **(1) The rev-walk engine** (`crates/sley-rev`, `sley-cli` log/rev-list/bisect) has a working core walk + reachability primitives but is missing the *history-simplification* layer and several walk modes; ~360 cells fail across t6002/t6018/t6019/t6111/t6012/t6016/t6021/t6600/t4202/t4216. **(2) The blame engine** (`sley-cli/commands/blame.rs`) exists but uses a first-parent-preserving line-router that diverges from git's diff-driven multi-pass blame — the whole t8xxx area (~230 fails, ~34%) collapses to one wrong-attribution root cause. Headline: TREESAME/simplification and reachability are *shared primitives* — fixing each unlocks many t-files at once; line-log and `rev-list --bisect` are unexposed/missing engines that already have most of their substrate in-tree.

### History-simplification engine (TREESAME modes: ancestry-path, simplify-merges, full-history, simplify-by-decoration, follow) — leverage: HIGH
- **Root cause:** `sley-rev/src/lib.rs:simplify_history` only implements `full_history` + `first_parent`; `--simplify-merges`, `--show-pulls`, `--ancestry-path` are explicitly stubbed (the doc at `lib.rs:2535` says they "are [not implemented]"). In `sley-cli/commands/rev_list.rs:108-118` `--simplify-merges`/`--show-pulls` are no-op match arms (`=> {}`); `--ancestry-path` and `--simplify-by-decoration` aren't parsed at all; `--follow` has no match arm in `log.rs`. The TREESAME machinery (`compute_treesame`, `tree_same_for_pathspec`) is real and correct for the default/`--full-history` cases — the gap is the *mode dispatch and the ancestry-path/merge-simplification post-passes* git runs in `revision.c:simplify_merges`/`limit_list` + the `ANCESTRY_PATH` marking.
- **Architecture:** Promote `simplify_history` into a real **history-simplification pass** over `SimplifyOptions` extended with `ancestry_path`, `simplify_merges`, `show_pulls`, `exclude_first_parent_only`. Mirror git's structure: (a) `--ancestry-path` = a reachability mark (commit is on a path from a `^excluded` bottom to a tip) computed with the existing `is_ancestor`/depth walks, intersected with the result set; (b) `--simplify-merges` = git's iterative `simplify_merges` fixpoint (collapse a merge to a parent when redundant, re-run until stable) layered on the existing `compute_treesame` + parent-rewrite that `simplify_history` already does; (c) `--simplify-by-decoration` = TREESAME against the *decoration set* (reuse the decorate-ref resolver). Wire all three into `rev_list.rs` and `log.rs` through one shared call so plain-log, rev-list, and the bloom path go through identical code. `--follow` is a separate rename-following pass (single-path, swaps the pathspec on a detected rename via `sley-diff-merge` rename detection) — same seam, follow it after the modes land.
- **Closes:** ~80 cells — `--ancestry-path` 42 (t6111: 20, t6019: 12, t4216: 10), `--full-history` 13 (t6012: 8, t6111: 5), `--simplify-merges` 11 (t6012: 6, t6111: 5), `--simplify-by-decoration` 10 (t4216), `--follow` 16 (t4202: 4, t4216: 10, t4206: 2), `--show-pulls`/`--exclude-first-parent-only` 4 (t6012). Plus the residual default-mode TREESAME cells in t6111 (cells 5-9) that share the parent-rewrite path.
- **Effort:** L · **Risk:** medium — TREESAME parent-rewrite is shared by every pathspec-limited log; `simplify_merges`'s fixpoint is subtle (git iterates to stability) and ordering-sensitive.
- **Depends on:** the existing `compute_treesame` (present) and rename detection in `sley-diff-merge` (present, for `--follow`).
- **Scope:** real-correctness

### Lift the bisection algorithm into sley-rev and wire `rev-list --bisect` — leverage: HIGH
- **Root cause:** The complete weighted-midpoint bisection engine (`do_find_bisection`, `count_distance`, `approx_halfway`, `filter_skipped`) already exists and is *correct* — t6030-bisect-porcelain passes 95/96. But it lives **privately inside** `sley-cli/commands/bisect.rs:1416-1490`, reachable only by `git bisect next`. `rev-list.rs` has **no** `--bisect`/`--bisect-vars`/`--bisect-all` handling, so all 49 t6002 cells and the t6000/t4205 `--bisect` cells fail despite the algorithm being shipped.
- **Architecture:** Extract `do_find_bisection` + the distance/weight helpers + `BisectTerms` reachability-set computation into a public `sley_rev::bisect` module (a shared primitive), keyed on the rev-walk's already-built reachable-commit list. Re-point `bisect.rs` at it (no behavior change — t6030 stays green) and add a `--bisect`/`--bisect-vars`/`--bisect-all` path in `rev_list.rs` that builds the `good..bad` reachable set via the existing `resolve_revision_range`/`walk_commit_metadata`, runs the shared finder, and formats the plumbing output (single midpoint, or `bisect_rev=`/`bisect_nr=`/`bisect_good=`/`bisect_bad=`/`bisect_all=` for `--vars`/`--all`).
- **Closes:** ~52 cells — t6002: 49 (`--bisect` 47, `--bisect-vars` 1, `--bisect-all` 1), t6000: 1 (`--bisect`), t4205: 1, plus the t6600 topo-bisect cross-checks.
- **Effort:** M · **Risk:** low — algorithm is proven; the work is extraction + output formatting. Main subtlety is the `^good` reachable-set boundary matching git's `--bisect` semantics exactly.
- **Depends on:** none (substrate present).
- **Scope:** real-correctness

### Blame engine: replace first-parent line-router with git's diff-driven multi-pass blame — leverage: HIGH
- **Root cause:** `sley-cli/commands/blame.rs:compute_blame` (479-586) routes each final line to *the first parent that preserves it* via a whole-line `child_to_parent_map` (Myers `Equal`-block map, `blame.rs:612`). git's blame instead carries **line chunks** through per-parent diffs (`blame.c:pass_blame`/`split_overlap`/`blame_chunk`), picks the *correct* parent for a merge (not the first preserving one), and handles `--contents`/working-tree, `-C/-M`, and `--reverse`. The first-parent heuristic is why merge-attribution cells fail ("2 merged-in authors", "evil merge", "blame --first-parent", "ancestor"/"great-ancestor"), and why `-L X,Y` *content* is wrong even though `-L` parsing is correct (error-path `-L` cells like `-L 0`, `-L X>nlines` pass; content cells fail). `--contents`, `-p/--porcelain`, `--incremental`, `-C/-M`, `--reverse` are hard-rejected in `is_unsupported_blame_option` (286).
- **Architecture:** Build a real **blame scoreboard** as a sley-rev/sley-cli engine mirroring `blame.c`: a `BlameEntry` list of `(commit, suspect-range, final-range)` chunks, a priority queue of suspects in commit-date order, and a `pass_blame` step that diffs the suspect's blob against each parent (reusing `sley-diff-merge` Myers/patience), `split_overlap`s the matched chunks down to the parent that actually contains them, and charges the residual to the suspect. This makes merge attribution correct by construction and naturally supports `-L` (it's just a final-range filter on the scoreboard), `--first-parent`, and later `-C/-M` (add cross-path diff passes) and `--reverse`. Add a porcelain emitter for `-p/--line-porcelain` and a `--contents`/working-tree source (route through `sley_worktree::apply_clean_filter` per the existing TODO at `blame.rs:120`).
- **Closes:** ~183 cells of the 230 t8xxx fails — the `-L range` content class (138) + core attribution/merge/output class (45) collapse to this one fix; t8001-annotate and t8002-blame are the same code (identical fail lists). Adds `--contents` (12) and `--first-parent` (3) as direct follow-ons; `-p`/porcelain and `--color-lines/-by-age` (t8012, 4) are emitter add-ons on top.
- **Effort:** L · **Risk:** medium — a from-scratch scoreboard, but self-contained to blame; no shared-engine blast radius. Output-format exactness (column widths, boundary `^`, date/email) is already mostly built in `render_blame`.
- **Depends on:** `sley-diff-merge` line-diff (present); `sley_worktree::apply_clean_filter` for `--contents` (present per TODO).
- **Scope:** real-correctness (the `-p`/`--incremental` porcelain emitters and `-C/-M` are follow-on, not harness-gaps).

### Line-log engine (`git log -L`) — leverage: MEDIUM
- **Root cause:** No `-L` handling anywhere in `log.rs` (grep for `-L`/`line_log`/`range_set` returns nothing). git's `line-log.c` is a distinct engine: parse `-L start,end:file` / `-L :funcname:file` / `-L /regex/`, seed a `range_set` on the final blob, then walk history tracking each range backward through diffs, emitting only commits that touch the tracked lines (with `-p`/`-s` rendering). Entirely absent.
- **Architecture:** New `line-log` engine, ideally sharing the blame scoreboard's chunk-tracking primitive (both track line ranges backward through per-parent diffs). Build a `RangeSet` type + a walk that, per commit, intersects the diff hunks against the live ranges, shrinks/relocates ranges across the diff, and emits the commit when a range overlaps a change. Reuse the rev-walk for ordering and `sley-diff-merge` for the per-commit diffs. The function/regex range forms (`:funcname:`, `/regex/`) need the funcname/regex range resolver (shared with blame's `-L /RE/`, which already works for the error cases).
- **Closes:** ~70 cells — t4211-line-log: 68 (`-L` 53 + parsing/rendering), t8012 `-L` overlap with blame. `-M` line-move-following (2 cells) is a follow-on.
- **Effort:** L · **Risk:** medium — new engine, but bounded; biggest shared win is co-designing the line-range chunk primitive with the blame scoreboard so both engines use one backward-range-tracking core.
- **Depends on:** the rev-walk (present) and ideally the blame scoreboard above (shared chunk-tracking primitive).
- **Scope:** real-correctness

### Reachability primitive library (`commit-reach.c` parity) — leverage: MEDIUM
- **Root cause:** `sley-rev` exposes only pairwise `is_ancestor` and `merge_bases` (`lib.rs:3985`, `4037`); the multi-commit reachability API git centralizes in `commit-reach.c` (`get_merge_bases_many`, `reduce_heads`, `can_all_from_reach[_with_flag]`, `get_reachable_subset`, `commit_contains`, `ahead_behind`, `get_branch_base_for_tip`) is **scattered and re-implemented per command** — `merge_rebase.rs:2787` has its own `merge_bases_many`, `reduce_heads` is inline-commented at `merge_rebase.rs:473`. t6600-test-reach is 0% because (a) these primitives aren't unified and (b) the test driver needs a `test-tool reach` shim sley doesn't have (`cmd_testkit` in `utility.rs:946` is a parity-fixture runner, not git's `test-tool`).
- **Architecture:** Consolidate one `sley_rev::reach` module exposing the full `commit-reach.c` surface over the existing `CommitGraphContext` generation-number pruning (already used by `is_ancestor` at `lib.rs:4002`). Re-point `merge_rebase.rs`/`rebase.rs` at it (dedup). The *real-command* payoff is `ahead_behind` (powers `git for-each-ref %(ahead-behind)` and `git branch -v` ahead/behind — t6600 cells 28-37) and `commit_contains` (`git branch/tag --contains` — t6600 16-17). t6600's `test-tool`-driven cells (the `can_all_from_reach`/`reduce_heads`/`get_reachable_subset` unit probes) additionally need a `test-tool reach` subcommand under `testkit` to exercise the library directly.
- **Closes:** ~30-48 cells — the for-each-ref ahead-behind + merged cells in t6600 (≈10) via real commands; the remaining t6600 cells need the test-tool shim (harness-gap). Indirect: hardens merge-base/rebase reachability (shared blast radius is a feature here — one correct library).
- **Effort:** M (library) + S (test-tool shim) · **Risk:** medium — shared by merge/rebase/branch; correctness regressions there would be costly, so land behind the existing merge-base tests.
- **Depends on:** none (generation-number pruning present).
- **Scope:** real-correctness (the library + `ahead_behind`/`commit_contains`); **harness-gap** for the `test-tool reach` unit-probe cells.

### Flagged out-of-scope / harness gaps
- **t6600 `test-tool reach` unit probes** (~20 cells: `can_all_from_reach`, `reduce_heads`, `get_reachable_subset`, `get_merge_bases_many`, `in_merge_bases*`, `get_branch_base_for_tip`) — these invoke git's internal `test-tool reach` directly; even cell 1 (setup) fails because the driver shells `test-tool reach`. **harness-gap**: needs a `sley testkit reach` subcommand wired to the reachability library; the underlying primitives are real-correctness (above).
- **t6021-rev-list-exclude-hidden** (~31 cells) and **t6018 `--exclude-hidden`** (33 cells) — `transfer.hideRefs`/`fetch.hideRefs`/`uploadpack.hideRefs`/`receive.hideRefs` config-driven pseudo-ref hiding. The `--exclude-hidden` flag *parses* (`setup.rs:362`) but the hideRefs **config integration** is server-protocol-adjacent. **real-correctness but low-priority** — the hideRefs namespace is normally a server concern; the local rev-list behavior is a thin config lookup, so it's a real gap, just not a core-walk one.
- **t4203-mailmap config plumbing** (~25 of 47 cells: `mailmap.file`, `mailmap.blob`, bare-repo `HEAD:.mailmap` default, blob-type fallback) — a real `Mailmap` engine exists (`utility.rs:570`), so this is **real-correctness config-integration**, but it's gated behind log/shortlog output being correct first (the `--use-mailmap` log/shortlog cells can't pass until the log output cluster does). Secondary to the engines above.
- **t4202-log `grep.patternType` + decorate-refs config** (~20 cells: `decorate-refs`/`decorate-refs-exclude`/`log.excludeDecoration`/`--clear-decorations`, grep.patternType) — **real-correctness** config plumbing on the decoration/grep path, independent of the walk engine; a separate config-wiring cluster, not architectural.
- **t4202/t6016 `--graph` layout** (~26 cells) — a graph renderer exists (`log.rs:graph_show_commit`); these are **real-correctness** layout edge-cases (merge rails, diff+stat interleaving under `--graph`), refinement of an existing engine, not a missing one.
- **blame `-C`/`-M` copy/move detection, `--reverse`, `-p`/`--incremental` porcelain** — **real-correctness** follow-ons to the blame scoreboard, not harness-gaps; sequence them after the core attribution rewrite lands.

---

## Diff, patch & apply engine

Assigned scripts t4013-various (97f/58%), t4015-whitespace (81f/40%), t4124-apply-ws-rule (70f/19%), t4014-format-patch (79f/64%), t4072-max-depth (48f/4%), t3206-range-diff (46f/4%), t7513-trailers (61f/38%), plus the broader t40xx/t41xx diff+apply family. Two structural absences dominate: **there is no whitespace-rule engine** (git's `ws.c`), and **`git apply` is a ~25%-complete unified-diff applier**, not a port of `apply.c`. The diff core (myers/patience/histogram, userdiff funcname, dirstat, word-diff) is solid — funcname/dirstat are at 100%, word-diff 97%. The gaps are in the *post-processing and apply* layers.

### A unified whitespace-rule engine (`ws.c` port) — leverage: HIGH
- **Root cause:** There is **no whitespace-rule subsystem anywhere** in sley. `git diff --check` returns `"diff check output is not supported"` (`sley-cli/src/commands/diff.rs:206-208`); `git apply` parses `--whitespace`/`--whitespace=` and **discards it** (`sley-cli/src/commands/plumbing.rs:1596-1610`, `cmd_apply` — the flag is matched and ignored, no fix/strip/error/warn action). The literal git error strings (`"trailing whitespace"`, `"space before tab in indent"`, `"indent with spaces"`, `"tab in indent"`) exist nowhere in the tree (grep empty). `sley-diff-merge` has zero `tab-in-indent`/`space-before-tab`/`tabwidth` logic.
- **Architecture:** Port git's `ws.c` as a new `sley-diff-merge::ws` module — the single correct-by-construction primitive that three callers share. Mirror git's surface exactly: `parse_whitespace_rule(&str) -> WsRule` (bitflags: `BLANK_AT_EOL|BLANK_AT_EOF|SPACE_BEFORE_TAB|INDENT_WITH_NON_TAB|CR_AT_EOL|TAB_IN_INDENT` + a 6-bit `TAB_WIDTH_MASK`, default `WS_TRAILING_SPACE|SPACE_BEFORE_TAB|8`); `ws_check_emit(line, rule) -> WsErrors` (powers `diff --check` output *and* `--ws-error-highlight` coloring in `render.rs`); and `ws_fix_copy(rule)` (powers `apply --whitespace=fix|strip`). The rule resolves from `core.whitespace` config + the `whitespace` gitattribute per-path (the `(attributes)` cells in t4124 are exactly this attr override). This is one module, ~370 lines like ws.c, feeding `diff.rs`, `render.rs`, and `cmd_apply`.
- **Closes:** ~70 cells t4124 (the entire `rule=trailing,space,indent,tab[,tabwidth]` combinatorial matrix + attributes), ~30+ cells t4015 (the `--check` family: trailing-space/space-before-tab/indent-with-non-tab on/off, tabwidth variants, `--check` interactions with `--exit-code`/`--quiet`, diff-index/diff-tree `--check`), ~16 t4019-diff-wserror, ~10 t4119-apply-config (`--whitespace=strip`), t4138-apply-ws-expansion (~4), t4040-whitespace-status (~4), t4029-diff-trailing-space (1). **~135 cells.**
- **Effort:** M · **Risk:** Low — additive new module; the bitflag rule + per-line check is well-bounded and git's ws.c is a clean reference. Blast radius limited to wiring three call sites.
- **Depends on:** gitattribute lookup (already exists — `attrs.rs`) for the `(attributes)` cells.
- **Scope:** real-correctness

### Promote `git apply` to a full `apply.c` engine — leverage: HIGH
- **Root cause:** `cmd_apply` (`plumbing.rs:1577`) is a thin wrapper over `sley_diff_merge::parse_unified_patch` + `apply_file_patch` (`sley-diff-merge/src/lib.rs:3624,3657`). The parser handles only `diff --git` unified hunks; `FilePatch` (lib.rs:3578) has no binary-patch, no copy, no stat fields. Missing entirely: `--stat`/`--numstat`/`--summary` rendering (t4100 0%), traditional/non-git patch parsing (t4135 weird-filenames 5%, t4119 traditional-input), `-p<n>` path component strip (t4120-popt 17% — `cmd_apply` swallows `-p<n>` as a no-op), subdir prefix handling (t4111 20%), `-R/--reverse` (hard-errors, t4116 14%), `-3/--3way` and `--index/--cached` (hard-error, t4108 22%), binary GIT-binary literal/delta patches (t4103 33%), `--whitespace=fix/strip` (t4138, t4119), ITA (t4140 0%). The apply doc-comment even claims it "mirrors git's apply.c" but only the exact-position hunk match is ported.
- **Architecture:** Build a real `sley-apply` engine modeled on git's `apply.c` state machine: a `Patch` IR with full git-header metadata (old/new mode, copy/rename src/dst, binary fragments, is_binary), a layered parser (`parse_git_header` → `parse_fragments` → `parse_binary`, plus `parse_traditional_patch` fallback per apply.c:856), and an apply driver with `-p<n>` strip, `--directory` prefix, `--reverse` (swap pre/post images), `--3way` (fall back to `merge_blobs`, which already exists in `sley-diff-merge`), and `--index/--cached` (write through `sley-index`). Stat/summary output reuses the existing diffstat renderer. The whitespace-rule engine (Opportunity 1) plugs in for `--whitespace=fix`. This is the structural seam: today apply lives as two functions inside the 7844-line `sley-diff-merge/lib.rs`; it should be its own crate/module with the `apply.c` shape.
- **Closes:** the t41xx apply family is **348 failing cells at 25%**. A full engine credibly closes the bulk: t4100 (25), t4104-boundary (21), t4129-samemode (18), t4135 (18), t4103-binary (16), t4108-threeway (14), t4128-root (11), t4114-typechange (11), t4120-popt (10), t4119-config (10), t4132-removal (10), plus t4102/t4109/t4111/t4116/t4122/t4126/t4140 etc. **~200+ cells across ~25 t41xx files** (some submodule/symlink cells defer to other clusters).
- **Effort:** XL · **Risk:** Medium — large surface, but git's apply.c is the exact spec; the existing fuzz/match-fragment logic is reusable. The 3-way path leans on the already-correct `merge_blobs`.
- **Depends on:** Opportunity 1 (for `--whitespace=fix`); `sley-index` (for `--index`).
- **Scope:** real-correctness

### Combined-diff (`-c`/`--cc`/`-m`) engine — leverage: MEDIUM
- **Root cause:** Combined-merge diff is explicitly stubbed: `diff_tree.rs:322-328` hard-errors `-c`/`--cc`/`--combined-all-paths` ("not supported"); `-m` only sets `merges_separate` (diff_tree.rs:331) and log.rs:2101 notes "first-parent diff... until separate/combined modes land." sley computes diff against one parent only — there is no N-parent diff combiner.
- **Architecture:** Add a combined-diff post-processor in `sley-diff-merge`: diff the merge result against *each* parent, then merge the per-parent hunks into git's combined format (`@@@`/`@@@@` headers, one prefix column per parent). Model on git's `combine-diff.c`. Wire `-c`/`--cc` (`--cc` collapses uninteresting parents) and `-m` (one full diff per parent) through `diff_tree.rs`, `log.rs`, `show.rs`.
- **Closes:** ~23 cells t4013 (the `-c`/`--cc`/`-m`/`whatchanged --cc` rows), t4038-diff-combined (20), t4048-combined-binary (11), t4057-combined-paths (4), and the `-c`/`-m` cells in t4072/t4069-remerge. **~60 cells.**
- **Effort:** L · **Risk:** Medium — combined-diff format subtleties (the `--cc` "interesting parent" pruning).
- **Depends on:** none (diff core exists).
- **Scope:** real-correctness

### Diff hunk post-processor framework: indent-heuristic + function-context + inter-hunk + `-I`/pickaxe — leverage: MEDIUM
- **Root cause:** `render.rs:render_hunks` owns hunk grouping with `context`/`interhunk` params, but several xdiff/diff-machine post-processors are absent: **no indent-heuristic** (slide hunk boundaries to nicer indentation — grep `indent.heuristic` empty; t4061 3%), **no function-context `-W`** (t4051 29% — grep empty in CLI), **inter-hunk-context** is validated but not applied at U0 (t4032 35%), and `-I<regex>` ignore-matching-lines (t4013 — 9 cells), `-S`/`-G` pickaxe (t4013 — 11 cells) are missing from the diff option pipeline.
- **Architecture:** Establish a **diff post-processor pipeline** between `diff_lines_with_algorithm` and `render_hunks`, mirroring git's `xdl_change_compact` (indent-heuristic) + `xdl_emit_diff` hunk shaping + the pickaxe/`-I` filters in git's `diffcore-*`. Each is a small composable pass over the `DiffOp`/hunk stream: indent-heuristic re-slides equal-line boundaries; function-context expands hunk ranges using the already-working userdiff funcname matcher (`userdiff.rs`); `-I`/pickaxe are diffcore filters that run before emission. The userdiff funcname engine being already at 100% means function-context is mostly *wiring* an existing matcher into hunk expansion.
- **Closes:** t4061 (32 indent), t4051 (30 function-context), t4032 (24 inter-hunk), t4013 `-I`/`-S`/`-G` (~20), t4033-patience/t4050-histogram boundary cells (~10). **~115 cells**, though spread thin per-pass.
- **Effort:** L (M each for indent-heuristic and function-context independently) · **Risk:** Low-Medium — indent-heuristic must byte-match git's slide algorithm; function-context reuses existing funcname.
- **Depends on:** userdiff funcname (exists).
- **Scope:** real-correctness

### format-patch series-tooling: `--base` / `--interdiff` / `--range-diff` + a range-diff engine — leverage: MEDIUM
- **Root cause:** format-patch has cover-letter/cover-from-description/reroll (`format_patch.rs`), but lacks `--base`/`--no-base`/`format.useAutoBase` (grep empty), `--interdiff`, `--range-diff`, and single-file `--output`. **`range-diff` is not a command at all** (grep `range.diff` empty in CLI) — t3206 is 4% because the whole subsystem is missing.
- **Architecture:** Build a `range-diff` engine in `sley-diff-merge` modeled on git's `range-diff.c`: match commits across two ranges by patch-id/cost-matrix, then emit the nested diff-of-diffs. `patch_id.rs` already exists to seed the matching. Once the engine lands, `format-patch --range-diff`/`--interdiff` and standalone `git range-diff` share it; `--base` is an independent small addition (compute/emit the `base-commit:` trailer + `prerequisite-patch-id` lines).
- **Closes:** t3206 (~40 of 46 — the rest are notes/submodule cells deferring to other clusters), t4014 cover-letter+interdiff+range-diff+base cells (~40 of 79 — the remaining cover-letter cells are output-format diffs that partly overlap Opportunity 1's whitespace and the base/cmd cells). **~80 cells.**
- **Effort:** L · **Risk:** Medium — range-diff's commit-matching cost matrix is subtle.
- **Depends on:** `patch_id.rs` (exists) for matching.
- **Scope:** real-correctness

### `git diff --max-depth` (t4072) — leverage: LOW-MEDIUM
- **Root cause:** `--max-depth` is entirely unimplemented for diff (grep finds it only in `grep.rs`, not in any diff command). t4072 is 4% pass purely from setup/error cells.
- **Architecture:** Add `--max-depth=<n>` to the diff-tree pathspec/recursion layer: when set, stop recursing into trees at depth N and emit the subtree boundary as its own entry (the test expects `one/two` as an entry at depth 1). This is a localized change to the tree-walk in `sley-diff-merge`'s `diff_tree_pair`/`collect_tree_entries` plus option parsing in `diff_tree.rs`/`diff_index.rs`/`diff.rs`. Note: `--max-depth` is disallowed with wildcard pathspecs (one test cell asserts this).
- **Closes:** ~46 of 48 cells t4072.
- **Effort:** M · **Risk:** Low — self-contained tree-walk option. (Niche feature; weigh against the higher-leverage engines above.)
- **Scope:** real-correctness

### interpret-trailers `--trailer`/config + `trailer.<key>.cmd` — leverage: LOW
- **Root cause:** `interpret_trailers.rs` has the `--where`/`--if-exists`/`--if-missing` state machine, but many cells fail on `trailer.<key>.cmd`/`.command` execution (which shells out to a user command per trailer), multiline-field atomic handling, and `trailer.<key>.*` config aliases. The `cmd`/`command` cells are the bulk of the "with cmd…" failures.
- **Architecture:** The config-alias + multiline-atomic + `--where`/`ifExists` completeness is real-correctness (port the remaining `trailer.c` `process_trailers` paths). The `trailer.<key>.cmd`/`.command` execution **shells out to an arbitrary external command** — flag as harness-gap-adjacent: it's an external-tool invocation, though git's own tests use trivial shell snippets so it's borderline implementable.
- **Closes:** ~35 of 61 t7513 (config/where/ifExists/multiline correctness); the ~26 `cmd`/`command` cells are external-command execution.
- **Effort:** M · **Risk:** Low.
- **Scope:** real-correctness (config/placement) + harness-gap (`cmd`/`command` external execution)

### Flagged out-of-scope / harness gaps
- **t4020-diff-external (50 fail)** — `GIT_EXTERNAL_DIFF` / `diff.external` runs an external diff tool; **harness-gap** (external-tool flow). Do not emulate. (Pre-flagged in the brief.)
- **t4030-diff-textconv (13), t4042-textconv-caching (7)** — `diff.<driver>.textconv` shells out to an external converter; **harness-gap** (external-tool). Related to t4020.
- **t7513 `trailer.<key>.cmd`/`.command` (~26 cells)** — executes an arbitrary user command per trailer; **harness-gap** (external-command execution), though git's test snippets are trivial enough to be borderline real-correctness.
- **t4041/t4060-diff-submodule-option (86 fail), t4027/t4059-submodule, t4137-apply-submodule (24), t4255-am-submodule** — submodule-specific diff/apply output; **defer to the submodule cluster** per the brief.
- **t4068-diff-symmetric-merge-base (33), t4069-remerge-diff (15)** — merge-base/remerge diff semantics; overlap the rev-walk and merge clusters more than the diff post-processors here.
- **t4150/t4151/t4152/t4153 am-* (mostly failing)** — `git am` mailbox flow; the apply-engine work (Opportunity 2) lifts the application core, but mailsplit/mailinfo parsing belongs to an am/mailbox cluster, not the diff engine.

**Highest-leverage lever:** the **unified whitespace-rule engine (Opportunity 1)** is the single best architectural fix — one ~370-line `ws.c`-shaped module closes ~135 cells across t4124/t4015/t4019/t4119/t4138/t4040 that are *currently impossible* because the primitive simply doesn't exist, and it's a hard dependency of the apply engine's `--whitespace=fix`. The **apply-engine port (Opportunity 2)** is the largest single bucket (~200+ cells) but is XL; it should be scoped as its own `sley-apply` crate with the whitespace engine landed first. Together these two close the bulk of the t41xx wreckage. The diff post-processor framework (Opportunity 4) is the right *shape* for indent-heuristic/function-context/inter-hunk/pickaxe — a composable pass pipeline between the LCS engine and the renderer — and is where the funcname engine (already 100%) finally pays off.

---

## Index, worktree, status, stash & sequencer (merge/rebase/cherry-pick/am)
Cluster spans ~30 t-files, ~1,179 failing cells. Headline: three engines dominate the leverage — a **real stash engine built on the merge primitive** (unlocks t3903 + the `--autostash` paths in merge/rebase), a **unified sequencer in-progress state** that `git status` reads (t7512 + conflict-status cells), and a **rebase "do-the-work vs noop" reflog discipline** (the entire 75-cell t3432 matrix is one bug). The index/worktree state machine is solid for cache-tree but missing the **untracked-cache (UNTR) / fsmonitor (FSMN) extensions** (t7063). Stash is *not* unimplemented (3,398 LOC) — it's architecturally wrong: a hand-rolled tree-restore that refuses dirty trees instead of merging.

### Rebuild stash apply/pop on the merge engine (read-tree -m / merge_recursive) — leverage: HIGH
- **Root cause:** `apply_stash_entry` (`crates/sley-cli/src/commands/stash.rs:243`) hard-refuses a dirty tree — line 263 returns `Unsupported("stash apply currently requires a clean working tree and index")` — and the moved-HEAD path (`apply_stash_tracked_paths_to_moved_head`, stash.rs:361) bails with `Unsupported` whenever a stashed path changed since the stash base (line 382). It restores by raw path-diffing + `fs::write` (`restore_stash_tree_entries_to_worktree`, stash.rs:440), never doing a 3-way merge. Git's `stash apply` does a `merge_recursive`/`unpack_trees` of the stash tree against the *current* worktree using the stash base as the merge-base (builtin/stash.c `do_apply_stash`). Every multi-step test cell (#8 "apply does not need clean working directory", #10/#11 apply, #19 pop, #14–#16 drop-then-apply, the symlink/rm/recreate chains #37–#51) trips one of these two `Unsupported` bails.
- **Architecture:** Route stash apply through the existing merge primitives instead of bespoke restore. sley already has both seams: `sley_diff_merge::merge_trees` (`crates/sley-diff-merge/src/lib.rs:4628`) and `sley_unpack_trees::{threeway_merge, unpack_trees}` (`crates/sley-unpack-trees/src/lib.rs:462,770`). Replace `apply_stash_entry`'s two restore branches with: build merge-base = stash parent[0], ours = current index/worktree, theirs = stash tree; run the same `three_way_merge_trees_*` helper that `merge_rebase.rs`/`replay.rs` already share, then write conflicts to worktree. Reuse `restore_index_paths_from_tree`/`reset_index_to_commit` (already imported) only for the `--index` reinstate step. Keep stash *create* (`create_stash_commit`, stash.rs:1135) — it correctly builds the 2–3-commit stash object; the gap is purely on the apply side.
- **Closes:** ~95 cells in t3903 (apply/pop/drop/branch/symlink/show families) **plus** 12 `--autostash` cells in t7600-merge (#15,54–66) and the autostash cells in t3432/t3401 once `create_autostash`/`apply_autostash` call the same engine. Net ~110 cells.
- **Effort:** L · **Risk:** medium — stash apply becomes a merge consumer, so conflict-marker/stage rendering must match; blast radius is contained to stash.rs since the merge engine is unchanged.
- **Depends on:** none (merge engine already exists). Autostash payoff depends on this landing first.
- **Scope:** real-correctness

### Unify the sequencer in-progress state and teach `git status` to read it — leverage: HIGH
- **Root cause:** Two parallel state machines — `sley_sequencer::replay` (`crates/sley-sequencer/src/replay.rs`, used by cherry-pick/revert) and `sley_sequencer::rebase` (`.../rebase.rs`, used by rebase/merge) — plus `am`'s own `.git/rebase-apply/` writer (`commands/am.rs:986`). `cmd_status` (`commands/workspace.rs:3526`) renders none of the "You are currently rebasing/cherry-picking/reverting/bisecting/am'ing…" in-progress headers; it only checks `CHERRY_PICK_HEAD`/`REVERT_HEAD` for *commit-author* purposes (workspace.rs:2367), not for status output. wt-status.c reads `wt_status_get_state` from these same on-disk markers + `rebase-merge/`, `rebase-apply/`, `sequencer/todo`, `BISECT_LOG`.
- **Architecture:** Add a single `SequencerState` reader in `sley-sequencer` that detects the in-progress op (rebase-apply vs rebase-merge-interactive vs cherry-pick/revert sequence vs am vs bisect) and the done/total step counts from the shared state dirs, then have `cmd_status` call it to emit the header block (git's `show_*_in_progress`). This makes the state *one* source of truth that all four backends write and status reads — a correct-by-construction invariant rather than per-command marker hunting.
- **Closes:** ~37 cells in t7512-status-help (rebase --apply/-i, am-session, cherry-pick/revert, bisect, split/amend/edit step counts) + the conflict-status cells in t7508 (#2/#3 conflict headers) + indirectly stabilizes t4151/t4153 abort cells that assert status mid-operation.
- **Effort:** M · **Risk:** low-medium — read-only status renderer over existing files; subtlety is matching git's exact "(N/M)" wording and rebase-i todo parsing.
- **Depends on:** none, but cleanest if the rebase/replay/am state dirs are reconciled to one schema first.
- **Scope:** real-correctness

### Rebase reflog "did-work vs noop" discipline for `--no-ff` — leverage: HIGH (single root cause)
- **Root cause:** All 75 t3432 failures are the identical assertion shape: `git rebase --no-ff` with no (or our) changes must **still do the work** — the reflog must grow (`test_line_count -gt`) — while landing on the **same HEAD OID** (`test_cmp_rev $oldhead $newhead`). sley's preemptive fast-forward in `rebase.rs:837–868` and the apply-backend FF at `rebase.rs:886` treat `--no-ff`/`--force` (mapped at rebase.rs:818, `--no-ff`→`force` at rebase.rs:162) by either short-circuiting to "up to date" (no reflog growth) or printing "forced" without re-picking. Git instead replays every commit (writing reflog entries) and, because trees + deterministic identity are unchanged, deterministically reproduces the same OIDs.
- **Architecture:** In both backends, gate the *FF short-circuit* on `!force`, and when `force` is set drive the normal pick loop even when `branch_base == orig_head`, so each replayed commit writes its reflog entry via the existing `detach_head_with_reflog`/commit path. This is one behavioral fix in the FF decision (rebase.rs:846/886) that closes the whole `--apply` + `--merge` matrix at once — the textbook close-the-class fix rather than 75 per-flag patches.
- **Closes:** ~75 cells, all in t3432-rebase-fast-forward (`--merge`:50, `--apply`:25).
- **Effort:** M · **Risk:** medium — touches the shared rebase FF decision used by every rebase; must not regress the genuine noop/up-to-date cases (#6 etc. currently pass).
- **Scope:** real-correctness

### Index untracked-cache (UNTR) + fsmonitor (FSMN) extensions and racy-clean refresh — leverage: MEDIUM
- **Root cause:** `sley-index` round-trips only the `TREE` cache-tree extension (`crates/sley-index/src/lib.rs:591–615`); UNTR/FSMN are never parsed or produced (grep finds no `UNTR`/`untracked_cache`/`FSMN` reader; `set_index_fsmonitor_valid_paths` at `sley-worktree/src/lib.rs:1279` is a stub that ignores its `_fsmonitor_valid` arg). So `git status` never writes/reads the untracked cache, and every t7063 cell that diffs the cache dump or asserts cache population fails. Separately, t7508 #56/#119–#125 (index refresh, `--no-optional-locks`, racy-clean fix for clean/dirty worktree) are the **racy-clean / stat-cache write-back** discipline (read-cache.c `refresh_index`/`mark_fsmonitor_valid` writing the smudged size + post-status index rewrite).
- **Architecture:** Add an `UntrackedCache` (and `FsmonitorExtension`) type to sley-index mirroring the `TREE`/`CacheTree` pattern (parse from `extension(b"UNTR")`, write via the same `encode_index_extension` path), and a `read_directory`-style untracked walker in sley-worktree that consults/invalidates per-directory cache entries on `.gitignore`/`info/exclude` change (the t7063 invalidation cells #15–#20). Pair with a status-end index write-back that smudges racy entries (size→0 trick) to satisfy t7508 racy cells.
- **Closes:** ~44 cells in t7063-status-untracked-cache + ~6 in t7508 (#56,119–125) + 2 in t2108-update-index-refresh-racy.
- **Effort:** L · **Risk:** medium — UNTR is a subtle binary format; getting the dump byte-exact (cells "verify untracked cache dump") is fiddly. Low correctness risk to other paths since it's an optional extension.
- **Scope:** real-correctness

### Standalone `git replay` plumbing command — leverage: MEDIUM
- **Root cause:** `commands/replay.rs` implements cherry-pick/revert (the *sequencer*), not the `git replay` porcelain-plumbing command. None of `--onto`/`--advance`/`--ref`/`--ref-action`/`--contained`/atomic-ref-update are parsed (grep finds only `--reference` for revert). t3650 (`replay-basics`) exercises this distinct command end-to-end; nearly all 41 cells fail because the command surface is absent.
- **Architecture:** A thin new `cmd_replay` that reuses the existing `three_way_merge_trees_styled` pick engine but emits the **ref-update plan to stdout** (replay's defining behavior — it doesn't move refs unless `--ref-action=update`), with `--onto`/`--advance`/`--revert`/`--contained` selecting the commit range. Build it on the same shared sequencer pick primitive so it doesn't fork a third replayer.
- **Closes:** ~36 real cells in t3650 (excluding #9 "replaying merge commits is not supported yet", which git itself rejects).
- **Effort:** M · **Risk:** low — additive new command; reuses the merge primitive.
- **Scope:** real-correctness

### Lower-leverage but real (grouped)
- **t7502-commit-porcelain (~50):** `git commit` summary oneline format (`[branch hash] msg` + ` N file changed`, cells #1–#3), `--trailer` interactions (#11–#24), commit-message cleanup modes (scissors/strip/verbatim, #31–#48), and `--status`/`commit.status` rendering (#62–#79). These are commit-command **rendering/cleanup** gaps, not engine gaps — medium effort, mechanical.
- **t2400-worktree-add (~67):** highest sub-cluster is `--orphan` DWIM inference (24 cells, #85–#110) and bare-repo add + tracking setup (#20–#23, #67–#78). Real-correctness, isolated to `commands/worktree.rs`; M effort.
- **t7600-merge non-autostash (~30):** `--squash` family (#14–#37), merge.stat/compact-summary rendering (#7–#10,33–35), `merge.ff=only`/log-message (#20,30,31,47). Rendering + squash-tree-no-commit semantics; medium.
- **t4150/t4151/t4152 am (~56 across files):** t4151 cascades from a failing `setup` cell (#1) — investigate the setup break first; am `--abort`/`--skip`/`-3` ride the `.git/rebase-apply/` state that the unified-sequencer-status work above also touches.

### Flagged out-of-scope / harness gaps
- **t3701-add-interactive (105) and t3404-rebase-interactive (93):** interactive-harness — do NOT emulate. Harness-gap, not correctness.
- **t7301-clean-interactive (21):** interactive `git clean -i` prompt loop — harness-gap.
- **t4257-am-interactive (3):** am `-i` editor/prompt flow — harness-gap.
- **t7900-maintenance (66):** `git maintenance` is a gc/repack/scheduler orchestrator (commit-graph, incremental-repack, prefetch, OS cron/systemd/launchd registration via `GIT_TEST_MAINT_SCHEDULER`). The scheduler-registration and prefetch cells are out-of-scope (servers/OS schedulers); the gc/repack-task cells belong to the **pack/odb** cluster, not this one. Flag the whole file as cross-cluster + partial out-of-scope.
- **t3600-rm (~25 of 31), t7002-mv-sparse-checkout (19), t3007-ls-files-recurse-submodules (22), t3512/t3513 cherry-pick/revert-submodule (24), t4255-am-submodule (28):** all **submodule-engine** dependent — belongs to the submodule cluster, not index/worktree/sequencer.
- **t3514-cherry-pick-revert-gpg (36), t7508 status -v/--column (#10,11,6,7):** GPG-signing and column/pager presentation — separate signing/output clusters.

**Biggest lever:** the **stash-on-merge-engine rebuild** (~110 cells incl. autostash payoff) edges out the **rebase no-ff discipline** (75 cells, but single trivial-ish root cause) and the **unified-sequencer status** (~40 cells, also unblocks am/cherry-pick status). A *real stash engine* is the bigger architectural lever than "a shared sequencer" — the sequencer is already ~70% shared (`three_way_merge_trees_*` is common; only the todo/state schemas are forked), whereas stash is the one major operation that bypasses the merge engine entirely and is the dependency for every `--autostash` path across merge/rebase.
