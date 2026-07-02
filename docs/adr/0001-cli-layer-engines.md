# ADR 0001 — CLI-layer engines, not per-command handlers

- **Status:** Accepted
- **Date:** 2026-06-13
- **Supersedes the framing of:** sley#8 (CLI god-module split)

**2026-07-01 update:** The facade notes below describe the ADR snapshot, not
the current state. `sley::Repository` now has active in-crate consumers and the
CLI setup layer documents the boundary it deliberately keeps outside
`Repository::discover`; command migration should treat the facade as an
available repo-intrinsic entry point, not as a dormant/bypassed experiment.

## Context

sley targets byte-for-byte parity with upstream git 2.54.0. The working method to date has been **gap-closing**: measure git's t-suite, find a failing t-file, hand-write whatever behavior makes its cells pass. This works, but it accumulates **per-command edge-case handlers** that drift — most visibly, the 2026-06-13 hardening branch shipped two regressions (`log_regex_unterminated_class_error` hand-matched git's exact regex diagnostic strings; `log_validate_output_indicator_for_log` hand-validated one option's empty-value behavior on a false premise). Those are not isolated bugs; they are symptoms of a structural pattern.

A three-axis architectural review of the codebase (command-flow/layering, edge-case-handler hotspots, engine/primitive coverage) reached a single, consistent conclusion.

### The three-tier finding

git's implementation has three tiers. **sley faithfully implements the first two and is missing the third.**

| Tier | git | sley today |
|---|---|---|
| **1 — Storage primitives** | object model, odb, packs, refdb, index | **Real, trait-seamed, used.** No raw `fs::write` to `refs/`; all ref writes go through `FileRefStore`/`FileRefTransaction` (334 references). |
| **2 — Compute engines** | revwalk, merge-ort, diff algorithms, pathspec, config | **Real and consumed.** `sley-rev::RevWalk` is documented as "the single seam every commit traversal consumes"; `sley-pathspec` is a byte-faithful `wildmatch.c` port; the 17 `diff_name_status_*` variants all funnel into one core. |
| **3 — CLI-layer cross-cutting engines** | `parse-options.c`, `diff_options`/`rev_info`, `strbuf_expand`+`date_mode`, `setup_revisions`, grep-source, `RUN_SETUP` flags | **Missing.** Every command (and often every *sub-mode*) re-implements these by hand. |

The absence of tier 3 is the entire "tacking on edge-case handlers" phenomenon. git factors the **CLI surface** once and every command inherits it (option negation, abbreviation, usage, `--name-only`/`--name-status` mutual-exclusion, `%`-placeholder expansion, date rendering, the rev-vs-path argv split, the "not a git repository" precondition). sley reproduces all of it per command. The consequences are measurable:

- `sley-cli` is **96k–108k lines — larger than all 23 engine crates combined (~85k).** A thin shell over engines inverts that ratio.
- `branch.rs` alone is **8,887 lines** with **6 hand-rolled option parsers**, each repeating the identical `--force=`/`--no-force=` "takes no value" ladder.
- There are **56 `parse_*_options` functions** and 100+ `while let Some(arg)` loops across the CLI, each re-deriving `=value` splitting, `--no-` negation, short-flag bundling, and `--` end-of-options.
- At this ADR's snapshot, the `sley::Repository` facade engine — the intended
  clean library entry point — had **zero CLI dependents**, leaving commands to
  wire low-level crates by hand. That observation has since been superseded in
  part: the facade is active inside the library, while CLI adoption remains a
  migration target where the repo-intrinsic boundary fits.
- Git *behavior* is trapped one tier too high: the unified-**patch renderer** lives in `sley-cli`, not `sley-diff-merge` (the library can *parse* a patch but cannot *produce* one); `log`'s notes + `--graph` logic is a CLI-resident state machine; the rebase interactive loop sits above a `sley-sequencer` engine that only holds data structures.

### Why this reframes the work

Each failing t-cell is not a missing *handler* — it is a missing *row in a tier-3 table*. "`log --foo` is unsupported" means `--foo` is not in `log`'s option spec, not that `log` needs a new code branch. Build the tables, and the option matrix becomes declarations instead of bespoke match arms. This is the mechanism by which **"build the engine, watch the capability family fall out"** replaces "tack on a handler." The empirical parity scan confirms it: failing t-files cluster by engine (the entire fetch/push/clone frontier is sub-50% together; rev-list options scatter 50–75% together) — capability families rise and fall together *because they share an engine*.

## Decision

**Invest in tier-3 CLI-layer engines, and migrate commands onto them — extracting the git behavior currently trapped in `sley-cli` down into the tier-1/2 engine crates as we go.** Concretely:

1. **Build the three missing CLI-layer engines** (ranked by leverage below).
2. **Migrate commands onto them in waves.** Each migration wave *homes that command's behavior in an engine* — the patch renderer into `sley-diff-merge`, log/graph formatting into a format engine over `sley-rev`, rebase orchestration into `sley-sequencer`. This is the real fix for the god-module; relocating functions within `sley-cli` (sley#8's current phases) moves zero behavior into engines.
3. **Adopt `sley::Repository` facade entry points where they fit the
   repo-intrinsic boundary** so commands stop hand-wiring low-level crates.
4. **Complete the two feature-incomplete compute engines** (revwalk simplification, transport coverage) as separate capability epics — *after* the CLI layer is table-driven, so they compose cleanly.

### The missing tier-3 engines (leverage-ranked)

**A — `parse-options` engine (the keystone).** A faithful port of git's `parse-options.c`: a per-command `&[OptionSpec]` table with typed values (`OPT_BOOL`, `OPT_INTEGER`, `OPT_MAGNITUDE`, `OPT_COLOR`, `OPT_CALLBACK`) and flags (`PARSE_OPT_NONEG`, `PARSE_OPT_OPTARG`), owning negation, abbreviation, short-flag bundling, `=value`, `--`, "takes no value" rejection, auto-generated usage/`-h`, and exit code 129 — uniformly, once. Subsumes the 56 hand-rolled parsers, the `--no-`/bundling micro-engines repeated across ~30 files, **and** the ~15 `log_validate_*` value-validators (one `Magnitude` type owns "expects an integer with optional k/m/g suffix" for every option that takes one, instead of `commit` and `log` cloning `*_validate_inter_hunk_context`). Highest capability-per-effort, lowest risk (the compute engines beneath are already solid).

**B — `diff_options` / `rev_info` + `setup_revisions`.** A shared diff-UI options struct (git's `struct diff_options`) with the output-format bitmask and the `--name-only`/`--name-status`/`-s`/`--check` mutual-exclusion check, consumed by `diff`/`log`/`show`/`stash`/`format-patch` instead of re-declared ~20 flags each. Paired with `setup_revisions` — the argv → `(revs, pathspecs)` splitter that owns the `--` boundary and the "ambiguous argument: unknown revision or path" diagnostic (re-implemented in 5+ commands today).

**C — Format substrate (`strbuf_expand` + `DateMode`).** One `%`-placeholder expansion substrate (padding, magic prefixes, alignment, atom dispatch) shared by the commit pretty-printer (`log_format.rs`, 1,168 lines) and the ref-filter atom formatter (`sley-ref-filter`, 1,506 lines) — today two independent engines. Plus a first-class, command-agnostic `DateMode { parse, render }` (git's `date.c`), retiring the awkwardly-borrowed `ForEachRefDateMode`. Folds in three smaller shared shims: a **grep-source/pattern model** (regex flavor + compilation + one diagnostic surface, shared by log/grep/for-each-ref — this is what the seed regex-error bug needed), a **pathspec-input collector** (`--pathspec-from-file` orchestration, re-coded in 7 commands), and **declarative command setup** (`RUN_SETUP`/`NEED_WORK_TREE` flags instead of 150+ hand-rolled "not a git repository" checks).

## Consequences

**Positive.**
- **Parity composes.** Adding an option = a table row; whole `--flag` matrices light up per engine instead of per t-file. The floor-chasing loop is replaced by engine investment.
- **The embedder story is fixed by the same move.** heddle and weft get clean *primitives* today but must re-derive git *behavior* on top of them. Promoting behavior into engines means the heddle→sley migration (heddle#595–#598) becomes "call sley's engines," not "re-implement git on sley's primitives." One architectural move serves both parity and the substrate migration.
- **The CLI shrinks toward a thin shell**, reversing the 96k-vs-85k inversion, and `fetch` (already a thin driver over `sley-remote`) becomes the template, not the exception.
- **Diagnostics stop drifting.** git's error strings and exit codes live in one engine, not 56 parsers — the class of regression the hardening branch shipped becomes structurally impossible.

**Negative / cost.**
- It is more up-front work than closing the next floor. The parse-options engine is load-bearing and must be faithful to `parse-options.c` semantics (abbreviation ambiguity, `OPTARG`, negation edge cases) before commands migrate.
- Migration is incremental and touches many command files; it must be sequenced so the tree stays green between waves (pilot a few commands, prove the engine, then fan out).
- Behavior extraction crosses crate boundaries (CLI → engine), which is higher-friction than in-crate code-motion — but it is the work that actually pays down the debt.

**Neutral.**
- This **retargets sley#8**: its diagnosis (oversized `lib.rs`) is real, but its remaining phases (a `Command` trait, splitting `branch.rs` into a submodule directory) are pure code-motion that relocate functions *within* `sley-cli`. They are superseded by "migrate commands onto tier-3 engines + extract behavior into tier-1/2 crates." sley#8 is reframed as the tracking issue for that migration.

## Implementation (epics)

- **Engine-epic A — `sley-options`** (parse-options + typed values). The keystone; everything else is easier after it. Built as a new crate with a small pilot migration (e.g. `branch.rs`'s 6 parsers) to validate the design before fan-out.
- **Engine-epic B — diff-options + `setup_revisions`** argv model.
- **Engine-epic C — format substrate + `DateMode`** (merge the two `%`-engines) + the grep-source, pathspec-collector, and command-setup shims.
- **Command-migration waves** (the retargeted sley#8): per command, declare its option table, route repo-intrinsic operations through `sley::Repository` where applicable, and extract its trapped behavior (patch renderer → `sley-diff-merge`; log/graph → format engine; rebase driver → `sley-sequencer`; recursive merge-base → `sley-diff-merge`).
- **Compute-completion epics** (after the CLI is table-driven): revwalk simplification modes (unlocks the rev-list option family); transport feature coverage (the fetch/push/clone frontier — and the `t5510-fetch` setup-crash bug).

## Evidence

This ADR is backed by a three-axis read of the codebase at sley `a972cef` (command-flow/layering; edge-case-handler hotspots; engine/primitive coverage). Key citations: `sley-rev/src/lib.rs` (`RevWalk`), `sley-refs/src/lib.rs` (`FileRefStore`/`FileRefTransaction`), `sley-diff-merge/src/lib.rs` (`merge_trees`) + `sley-cli/src/commands/merge_rebase.rs` (`virtual_ancestor_entry_map`, the stranded recursive-base strategy), `sley-cli/src/commands/args.rs` (mechanical-only option helpers — the gap), `sley-cli/src/lib.rs` (`log_validate_*` cluster, the patch renderer), `sley-cli/src/log_format.rs` vs `sley-ref-filter/src/lib.rs` (the two `%`-engines), and `crates/sley/src/lib.rs` (the then-zero-CLI-dependent `Repository` facade).
