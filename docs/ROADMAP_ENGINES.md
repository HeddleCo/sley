# Engine roadmap (upstream / parity clusters)

Short, actionable map of **where parity work should land**. Capability families
rise and fall together because they share engines — invest in the engine, not
one-off CLI handlers. Framing: [ADR 0001](adr/0001-cli-layer-engines.md).
Streaming I/O rules: [ADR 0002](adr/0002-streaming-io-house-rule.md). Living
status and floors: [`TRACKER.md`](../TRACKER.md), [`PARITY.md`](../PARITY.md).

## How to use this doc

1. Find the failing t-file / cell cluster in the testkit reports.
2. Map it to a row below (engine first, command second).
3. Prefer table rows / shared options / shared streams over new `while let Some(arg)` arms.
4. Keep CLI as argv, setup, hooks presentation, and byte-identical stdout/stderr.

---

## CLI-layer engines (tier 3 — keystone)

| Engine | Crate / home | Next actions | Upstream signal |
|---|---|---|---|
| **parse-options** | `sley-options` | Pilot migrations already include pack/refs/ls-remote; fan out remaining `parse_*_options` (branch, commit, log, diff family). Keep diagnostics + exit **129** in the engine. | Option matrices, `--no-` / abbrev / bundled shorts |
| **Command registry / help** | `sley-options::CommandRegistry` + `sley-cli` help | Keep declarative flags for builtin/main/porcelain; drive completion helpers from `OptionSpec` tables. | `git help`, completion, usage blocks |
| **diff_options / rev_info** | `sley-rev` (+ CLI glue) | One shared diff-UI bitmask + mutual exclusion (`--name-only` / `--name-status` / `-s` / `--check`); shared by diff/log/show/stash/format-patch. | `t40xx` / `t42xx` option scatter |
| **setup_revisions** | `sley-rev` | Single argv → `(revs, pathspecs)` split; own `--` and “unknown revision or path” diagnostics. | log/rev-list/diff pathspec boundary bugs |
| **Format substrate** | `sley-pretty` + `sley-ref-filter` + `sley-strbuf-expand` | Merge `%`-placeholder expansion; first-class `DateMode`; shared grep-source diagnostics. | log `--format`, for-each-ref atoms, date modes |
| **Declarative setup** | CLI session / future flags | `RUN_SETUP` / `NEED_WORK_TREE`-style tables instead of 150+ hand-rolled repo checks. | “not a git repository” / worktree gates |

**Pilot note:** `sley version` and several pack/ref commands already go through
`sley_options::parse_options`. Prefer that path for new CLI surfaces.

---

## History / graph (tier 2)

| Engine | Crate | Next actions | Upstream signal |
|---|---|---|---|
| **Revwalk** | `sley-rev` | Simplification modes (`--full-history`, `--simplify-merges`, path limiting completeness); exclude walks on the fast metadata path where still missing. | `t60xx` rev-list / log ordering & simplify |
| **Rev-parse / object names** | `sley-rev` | Edge selectors (`@{…}`, tree-ish:path remaining gaps), consistent peeling vs symbolic resolution at the facade. | `t15xx` / rev-parse cells |
| **Commit-graph** | `sley-odb` / `sley-rev` | Keep generation/walk acceleration; verify write/read parity under maintenance. | commit-graph t-files, log/rev-list perf |

---

## Worktree / index / unpack (tier 2)

| Engine | Crate | Next actions | Upstream signal |
|---|---|---|---|
| **unpack-trees** | `sley-unpack-trees` | Complete n-way merge entry points; gitlink/submodule move-head hooks; sparse / skip-worktree arms. See [spike](spikes/unpack-trees-engine.md). | checkout/switch/restore/reset/read-tree/merge clusters |
| **Index I/O + projection** | `sley-index`, `sley-worktree` | Sparse index, untracked-cache, fsmonitor integration already partial — close remaining cache invalidation edges. | `t30xx` / `t7xxx` status & index |
| **Checkout / status plans** | `sley-worktree`, facade `StatusPlan` | Keep streaming status emit (`StreamControl`); avoid buffering whole trees for short-status. | status performance + sparse |

---

## Storage / pack (tier 1–2)

| Engine | Crate | Next actions | Upstream signal |
|---|---|---|---|
| **ODB / pack install** | `sley-odb`, `sley-pack` | Streaming install only (ADR 0002); geometric repack / cruft / bitmap edges still open. | `t53xx` pack, midx, bitmaps |
| **Reachability / fsck** | `sley-odb`, `sley-fsck` | Connectivity completeness; promisor-aware missing objects. | fsck / partial clone |
| **Refs** | `sley-refs` | reftable backend (non-blocking for many consumers); transaction/CAS edges. | refs migrate / packed-refs races |

---

## Transport / remote (tier 2 + streaming)

| Engine | Crate | Next actions | Upstream signal |
|---|---|---|---|
| **Protocol codecs** | `sley-protocol` | Keep sideband as `Read` adapter; v0/v1 residual; server receive-pack edges. | `t55xx` / protocol tests |
| **Fetch/push/clone orchestration** | `sley-remote`, `sley-fetch` | Partial clone on-demand, unshallow, signed/atomic push gaps; always wire `cancel` + `drain_to_end`. | fetch/push/clone frontier &lt;50% clusters |
| **Transport / credentials** | `sley-transport` | Credential cache/daemon sandbox constraints; helper protocol completeness. | `t03xx` credential |
| **Facade** | `sley` (`remote` feature) | Prefer `sley::clone_repository` / `Repository::{fetch,push}_with_cancel` over ad-hoc `sley_remote` wiring in embedders. | heddle / library consumers |

---

## Diff / merge / sequencer

| Engine | Crate | Next actions | Upstream signal |
|---|---|---|---|
| **Diff algorithms + rename** | `sley-diff-merge` | Keep single core for name-status variants; complete patch **render** extraction from CLI. | diff/rename t-files |
| **Merge-ort / tree merge** | `sley-diff-merge` | Recursive base / virtual ancestor completeness. | merge conflict matrices |
| **Sequencer** | `sley-sequencer` | Move interactive rebase driver logic down from CLI; state machine parity. | rebase/cherry-pick/revert |

---

## Sequencing (suggested order)

1. **parse-options fan-out** on high-churn CLI modules (ADR 0001 epic A).
2. **setup_revisions + diff_options** shared by log/diff/show (epic B).
3. **unpack-trees completion** for checkout/reset/merge worktree application.
4. **Transport / partial-clone** features behind streaming house rule (ADR 0002).
5. **Format substrate** merge (pretty + ref-filter) once option tables stabilize.
6. **Revwalk simplification** as a compute epic once consumers share one options model.

---

## Non-goals for this roadmap

- Relocating functions inside `sley-cli` without extracting behavior to engines.
- Buffering full pack/sideband responses “for simplicity.”
- Per-command private option micro-parsers when `sley-options` already covers the shape.
