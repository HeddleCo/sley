# First-principles architecture audit

Recorded on 2026-09-07 against Sley 0.9.0, `38b8b5cd`. The workspace map and
finding/evidence structure follow [Heddle's audit on its clean-cut
branch](https://github.com/HeddleCo/heddle/blob/bc029f475faae3eda5182d4d2ac63ed3a8c6e01a/docs/program/ARCHITECTURE_AUDIT.md).

## Core intent and verdict

Sley's irreducible job is to understand and exchange Git data: object identity
and encoding, graph reachability, repository formats, object/pack storage,
transactional refs, and Git transport. Embedders must be able to use those
operations without adopting a Git working tree or Sley's CLI. Heddle owns its
native model and its projection onto Git; Weft owns its hosted storage and
network policy. Sley owns the Git semantics between them and Git peers.

Correctness is anchored in an independent, pinned upstream oracle. A useful
Git implementation must preserve Git behavior even when that behavior is
awkward. Deleting an old Git format reader, a protocol version, or an enrolled
command is not the same as deleting an obsolete Rust implementation.

**Sley has a sound storage/transport foundation, but it is not already lean as
an embeddable library.** It has inherited the broader ambition of a complete
Git replacement, exposes much of the resulting machinery publicly, and still
carries assumptions from Git's one-repository-per-process execution model.
The safe deletion set is modest. A large clean cut would mostly remove live
parity behavior or published APIs, so it belongs in a coordinated follow-up.
This PR removes dead copies and duplicate implementation; it changes no public
signature, feature, default, crate membership, or oracle acceptance rule.

## Workspace map and dependency direction

There are **36 workspace crates; 31 are publishable**. Counts below are physical
Rust lines under each crate's `src/` at the baseline, including inline tests and
comments, excluding separate integration tests and benches. They measure code
ownership, not production instruction count.

| Area | Crates | Source lines |
|---|---|---:|
| CLI and command behavior | `sley-cli` | 199,072 |
| Worktree semantics | `sley-worktree` | 35,797 |
| Remote orchestration | `sley-remote` | 31,967 |
| Diff/merge | `sley-diff-merge` | 27,147 |
| Object database | `sley-odb` | 19,810 |
| Revision selection/walk | `sley-rev` | 19,503 |
| Wire codecs | `sley-protocol` | 16,747 |
| History editing | `sley-sequencer` | 15,002 |
| Packs | `sley-pack` | 14,832 |
| Ref storage/transactions | `sley-refs` | 11,260 |
| Facade | `sley` | 7,760 |
| Primitive types and process policy | `sley-core` | 7,187 |
| Objects | `sley-object` | 2,233 |

```text
heddle / weft                  sley-cli
        \                     /      \
         sley facade         /        CLI-only consumers: archive, gc, i18n
          |                 /
          +-- remote orchestration -> transport + protocol
          +-- worktree / sequencer / pretty / revision engines
          +-- refs / ODB -> pack / formats -> object / config -> core

sley-testkit + upstream Git -> separate oracle/candidate runs -> TAP comparison
```

This is an ownership sketch, not every Cargo edge. The manifest graph has 29
Sley crates reachable from the facade when optional remote support is included,
and **26 even following only non-optional normal dependencies**. Turning off
`remote` does not remove worktree, sequencing, mail, pretty printing, or option
parsing. `sley-rev` depends on `sley-options`; `sley-object` depends on config for
identity resolution; `sley-formats` contains tree rendering as well as codecs.
The names do not describe cleanly separated embedding layers.

No dead crate was established. The smallest crates have live, distinct roles:
`sley-mmap` contains the unsafe mapping boundary; `sley-procinfo` supplies CLI/GC
process information; `sley-strbuf-expand` serves pretty/ref formatting;
`sley-options` serves revision parsing as well as the CLI. Non-published
`sley-archive` and `sley-gc` have live CLI callers. `sley-testkit` and `sley-bench`
are executable verification tools, not unused runtime dependencies.

## First-principles findings

### 1. Git replacement breadth has become an implicit embedding requirement

[`GOAL.md`](../../GOAL.md) asks for a complete drop-in Git replacement, including
porcelain and maintenance. That explains the CLI's size; it does not establish
that every embedder needs those dependencies or that every internal engine
helper should become a stable library API. The README's former description of
a thin CLI was not supported by the current code. It now states the boundary
honestly.

The wrong response would be to delete mail, rebase, Scalar, archive, or helper
adapters because Heddle does not need them. They serve the existing Git surface,
including the curated oracle. The useful cut is between the embedding contract
and the compatibility executable, preserving all enrolled behavior.

### 2. Public convenience surfaces sometimes promise future work

[`StatusCacheKey`](../../crates/sley/src/status_plan.rs) is stored and returned,
but never consulted by status execution. `reuse_index_cache` does not implement
cache reuse. [`BlobStore::read_or_fetch`](../../crates/sley/src/objects.rs) is an
async wrapper around a synchronous local read; choosing a remote changes the
missing-object context, not whether objects are fetched. These are speculative
API commitments, not implemented capabilities.

[`RepositoryCapabilities`](../../crates/sley/src/capabilities.rs) mixes support
with repository state: `current().shallow` is true, while
`Repository::capabilities().shallow` reports whether this repository is shallow.
Several other fields are unconditional true values.

Source searches across the pinned Heddle and Weft snapshots found no callers of
`reuse_index_cache`, `StatusCacheKey`, `read_or_fetch`, `RepositoryCapabilities`,
or `transport_capabilities`. That is useful migration evidence, not permission
to remove publicly exported APIs. These remain unchanged. The reachable-pack
plan is different: it freezes real object selection/order and has a Heddle
hosted-client test caller; it was not classified as speculative.

### 3. Process-global repository policy conflicts with embedding

[`sley-core::namespace`](../../crates/sley-core/src/namespace.rs) stores a
process-wide override and otherwise reads `GIT_NAMESPACE`. Local advertisements,
receive-pack ref updates, and push hide-ref checks consult it. The Unicode
precomposition flag in [`precompose.rs`](../../crates/sley-core/src/precompose.rs)
is also process-wide. `ORIGINAL_CWD` in `sley-core` can be initialized only once.
Transport policy still consults `GIT_ALLOW_PROTOCOL` and
`GIT_PROTOCOL_FROM_USER` inside library operations.

These choices can be faithful to Git's CLI yet unsuitable for two independently
configured repositories in one process. Mutexes and atomics avoid data races;
they do not establish repository ownership. Removing the globals without
threading their values through all affected operations would change Git
behavior and break callers using those controls. Keep behavior here, move its
ownership in the coordinated cut.

### 4. Diagnostics and public exports cross the intended layer boundary

The facade's [`plumbing`](../../crates/sley/src/lib.rs) exports make whole engine
crates accessible, alongside many root aliases. Those are real public contracts,
including for consumers depending only on `sley`. Missing workspace callers do
not prove an engine function is dead.

[`GitError`](../../crates/sley-core/src/lib.rs) still includes both `Io(String)`
and `IoKind`, plus `Cli` and `Exit`. Some ODB/revision helpers print diagnostics
directly, for example corrupt-index fallback in
[`registry.rs`](../../crates/sley-odb/src/registry.rs). Deleting those prints would
lose byte-visible oracle behavior; leaving them unconditional constrains hosts.
Existing typed outcomes and explicit warning sinks are the direction to extend.

### 5. The 0.9 object-reader push boundary is the right abstraction

[`HttpPushActionsRequest<R>`](../../crates/sley-remote/src/push.rs) takes
`R: ObjectReader + Sync`; the filesystem push entry point adapts its ODB into
that path. The transport-neutral pack/body writer also takes `ObjectReader`.
That is an actual storage boundary, with no requirement to manufacture a bare
repository merely to push from another store.

The single-use `HttpReceivePackObservation` is load-bearing. It binds the remote,
format, capabilities, advertised refs, and HTTP client; execution validates
the intended old ids and sends those same receive-pack CAS preconditions.
One observation does not prevent a later remote race; the server-side CAS does.
Do not replace this with a cloneable ref list or rediscover after reconciliation.
SSH and `git://` plans own live sessions. Extending virtual-store support to them
must preserve that session ownership; it is not a reason to generalize away the
HTTP observation or break the just-adopted seam.

### 6. The oracle is essential, and its present guarantee has limits

[`upstream-parity.yml`](../../.github/workflows/upstream-parity.yml) pins Git
2.55.0, runs oracle and candidate separately, requires a clean oracle baseline,
records exact TAP comparisons, and enforces per-script pass floors. PRs select
13 scripts; scheduled/manual runs select the 891-script curated manifest.
[`upstream-parity-matrix.yml`](../../.github/workflows/upstream-parity-matrix.yml)
retains platform/hash coverage and an optional strict correctness gate.

The default gate is regression floors, **not a proof of complete parity**.
Equal passing-cell counts can hide a change in which cells pass; the exact
comparison artifacts remain necessary review evidence. The 100% readiness
report is advisory in the regular workflow. Windows floor calibration is still
explicitly unresolved in [`parity-gates.md`](../parity-gates.md).
No workflow, selection, floor, waiver, prerequisite, test, or comparison rule is
removed or weakened in this PR. Future embedding changes must keep the same
compatibility executable and oracle surface, and should add direct library
coverage where process-isolated CLI tests cannot prove embedding properties.

## Implemented now

| Change | Evidence and reason |
|---|---|
| Delete `sley-cli/src/tree_print.rs` (307 lines) | No module declaration, include, build target, or compiler dependency names this file. `sley-cli/src/lib.rs` already imports `sley-formats::tree_print::*`; `ls-tree` and `cat-file` call that live engine. The removed file contains no tests. |
| Delete `sley-cli/src/commands/workspace.rs` (1 line) | Only a comment remains; no module declaration or build target. Historical extraction left the stub behind. |
| Remove `encoding_rs` and `flate2` from CLI dependencies | `cargo machete` identifies both; source search confirms no code imports. The only CLI `flate2` text is a profiler output label. Encoding and binary-patch compression remain in their engine crates. Remove the two CLI lockfile edges, not the still-used packages. |
| Consolidate ODB construction | `new` still discovers alternates; `without_alternates` still does not. Both pass the same explicit alternate set into one private initializer, preserving per-pack budget calculation and every cache/replacement/promisor field. |
| Delete duplicate repack checksum validator | Its 32-line function is identical to the existing crate-private installer validator. Repack/cruft callers now import that implementation, retaining all validation and diagnostic contexts. The separate file-streaming validator stays. |
| Remove unused registry-scan format input | Registry scanning records paths; parsing occurs lazily with the database format. Delete the unused private parameter and correct its comment. |

The implementation and lockfile diff is **15 lines added, 380 removed (365 net
removed)**. Of these, 308 lines are entire dead files; the rest remove dependency
edges and duplicated implementation. No crate or test was deleted. Documentation
line counts are separate.

After deletion, a whole-repository path/name sweep found only the live engine
tree printer, the shared checksum validator, and historical references to the
workspace stub in the July review and pre-alpha plan. Those historical records
remain as evidence of the earlier state; no gate references a removed path.
Compiler dependency files were checked as well as source text, and the explicit
Scalar binary, profiling features, benchmark binary, and test-only modules were
excluded from the dead-file candidates after inspecting their target/cfg wiring.

## Deliberately retained

- Git format and transport compatibility: SHA-1/SHA-256, loose and packed
  objects/refs, reftable, shallow/promisor/replacement semantics, protocol v0/v1/v2,
  thin packs, and old-id ref preconditions. These represent external Git data
  and behavior, not Rust migration baggage.
- Streaming I/O, cancellation, size/delta budgets, quarantine, checksum/index
  validation, pack-last publication, and ref transactions. They protect real
  operations; simplifying them requires equivalent behavior, not fewer checks.
- ODB pack/MIDX/decoded-object caches and mmap isolation. Existing tests cover
  refresh after misses, alternates, corrupt-copy fallback, bounded eviction, and
  prepared-versus-buffered pack order. No evidence supported deleting them.
- The CLI, shell/helper provenance, i18n, mail, maintenance, and all oracle/testkit
  machinery. They have live compatibility/verification duties even when they
  are outside the smallest useful embedding dependency graph.
- Public aliases, constructors, plan types, errors, and the complete 0.9 push
  seam. Source-level absence of consumer calls is not a semver compatibility
  proof. Neither consumer is repinned by this PR.

## Ranked breaking cuts — proposals only

Every cut below needs a breaking release and a consumer migration review. None
is implemented here. Ranking is by embedding value and coordination risk, not
by the largest possible diff.

| Rank | Proposed cut | Consumer ripple | Sequence relative to heddle#1718 |
|---|---|---|---|
| 1 | Replace process-global namespace/Unicode/CWD policy and ambient transport-policy reads with values owned by an operation or repository. Remove the corresponding global setters after migration; the CLI resolves its environment once and passes equivalent values. | Heddle's repo/projection operations need explicit policy; Weft's concurrent workers and HTTP policy adapters must not share another operation's namespace or config. Add two-repository, concurrent library tests, plus unchanged CLI namespace/Unicode oracle cases. | Preserve 0.9 while #1718 lands. Introduce explicit alternatives first, migrate both consumers, then remove the globals in the breaking release. Do not bundle a change to the HTTP observation seam. |
| 2 | Cut the facade's mandatory porcelain dependency closure and broad `plumbing::*` commitment. Keep a focused embedding surface; make worktree/history-editing/rendering an explicit opt-in surface, with all features enabled for the compatibility CLI. Do not merge crates merely to reduce the count. | Both consumers import through `sley::plumbing`; Heddle also uses config, diff/merge, refs, notes, and objects outside git-projection. Weft storage/registry/import code uses pack and object APIs, and workers/base depend directly on `sley-transport`; Weft also directly pins `sley-mmap`. Removing exports or changing defaults requires import/feature edits throughout both workspaces. | After the paired 0.9 adoption, inventory the used symbols and compile both workspaces against proposed features. Remove/relocate exports only with the paired breaking version; keep all 891 oracle scripts enrolled. |
| 3 | Delete speculative `StatusCacheKey`/`reuse_index_cache` and fetch-named local-only blob methods; remove or replace capability flags that mix support and state with existing concrete queries. Do not add caching or networking just to justify the names. | No references to these names were found in either pinned consumer's Rust source. Still check wrappers and published downstream clients before removing the exported types/methods. Callers needing blobs can use local reads and a real explicit hydration operation. Keep the functioning reachable-pack plan. | Low migration burden, but still wait for a declared breaking release after #1718. This can share the facade cut; it must not unexpectedly land in a 0.9 patch. |
| 4 | Remove CLI exit variants and the duplicate string-only I/O channel from the library error contract; return diagnostics through caller-owned outcomes/sinks while retaining CLI byte rendering. | Heddle's credential classification explicitly matches `GitError::Io`; Weft's `weft-base/src/ssrf.rs` constructs it repeatedly. Those sites must migrate to typed errors. Other exhaustive matches and rendered-message checks need review. Preserve their custom HTTP client and authentication behavior. | Migrate errors and sinks additively first. Land the removal only after both consumers compile and their error-path tests pass; retain cancellation and sideband-fatal distinctions and run diagnostic parity tests. |

## Consumer snapshot and release sequence

Consumer evidence was read through HTTPS GitHub API snapshots, without reading
or modifying other worktrees:

- [Heddle #1718](https://github.com/HeddleCo/heddle/pull/1718), open at audit time,
  branch `codex/agent-native-vcs`, head
  `bc029f475faae3eda5182d4d2ac63ed3a8c6e01a`. Its workspace requests Sley and
  sley-transport 0.9.0. `crates/git-projection/src/git_core.rs` observes
  receive-pack, reconciles refs, and passes its object reader to
  `push_http_actions_with_reader_from_observation`.
- [Weft main](https://github.com/HeddleCo/weft/tree/1c2dee83fe4cd05bcf9501a019ddcf97e5f4ce79),
  `1c2dee83fe4cd05bcf9501a019ddcf97e5f4ce79`, still pins `=0.8.0` in its
  manifests and lockfile. The 0.9 seam is available to Weft, but adoption on
  main was not established. Heddle's `docs/WEFT_CLEAN_CUT_HANDOFF.md` explicitly
  asks Weft to adopt it with the paired model cutover.

Land this nonbreaking audit independently. Let #1718 complete on the released
0.9 seam, and coordinate Weft's paired Heddle/Sley update. Then stage the explicit
embedding-policy and facade alternatives, validate both consumers, and ship a
declared breaking Sley release with their corresponding repins. No follow-up
should make #1718 chase an unannounced transport API or silently leave Weft on a
different object/error contract.

## Verification

All Cargo commands use `CARGO_TARGET_DIR=/home/scratch/sley-audit-target` and
`TMPDIR=/home/scratch`. The installed oracle reports `git version 2.55.0`.

- `cargo build --workspace` passes after the implementation changes.
- `cargo test --locked -p sley-odb`: 129 passed, zero failed.
- Negative check: temporarily disabling the shared pack-checksum comparison
  makes `cruft_prefix_installer_validates_every_component_before_mutation` fail
  because the damaged pack is installed. Restoring it passes, including in the
  full ODB suite. This demonstrates that removing the duplicate did not bypass
  the live validation path.
- `cargo machete` reports no unused dependencies after the two CLI edges are
  removed. `git diff --check` and scoped nightly rustfmt pass.
- `cargo test --locked --workspace`: 3,667 passed, zero failed, two ignored
  across 209 test targets (including doctests). This includes the touched CLI,
  nine `ls_tree` tests, eight `cat_file` tests, and 189 remote unit tests.
- `cargo clippy --all-targets -- -D warnings`, the CI workspace
  `--all-features --no-deps` variant, and `sley-remote --no-default-features`
  Clippy all pass.
- The CI panic gate, publish-list checker (31 crates), and
  `RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --lib`
  pass.
- Upstream manifest validation confirms 891 scripts; environment preflight
  passes. The release binaries were built with
  `cargo build --locked -p sley-cli --bins --release --features git-compat-i18n`.
- The unchanged 13-script PR selection passes the Linux floor gate:
  `check-parity-floors.sh pr-summary.csv .github/workflows/parity-pr-scripts.txt`.
  Its raw runner exits 1 for expected partial parity, as in CI; the floor gate
  exits 0. No result was removed from that run's summary.

### Extended parity and unchanged-base comparison

An additional run used all 13 PR scripts plus `t3103-ls-tree-misc.sh`,
`t5300-pack-object.sh`, `t5319-multi-pack-index.sh`,
`t5329-pack-objects-cruft.sh`, and `t5613-info-alternate.sh`. Both targets used
the same built Git 2.55.0 source, Linux/SHA-1, eight waves, and a 900-second
per-script timeout through the unmodified upstream runner.

| Result | Observation |
|---|---|
| Oracle | 17 passing scripts, one legitimate skip, zero failures/aborts/timeouts |
| Audited Sley | 13 passing scripts, one skip, four partially failing scripts; zero aborts/timeouts |
| Exact oracle vectors | 13/18; this is not 100% compatibility |
| Extended floor gate | **Fails** solely on `t5613-info-alternate.sh`: `ok=9`, floor 10 |
| Unchanged base `38b8b5cd` | Same extended floor failure, same per-script summaries, and **all 1,837 TAP cells identical** to the audited code |

The base was built from `git archive 38b8b5cd` into a task-owned scratch
directory, using the same isolated Cargo target. The audited release binaries
were preserved before building the base. Comparison checked script/cell ids,
status, raw result, directive, and description, not just aggregate counts.
`t5613` fails cells 2, 3, 7, and 12 in both builds. This is a pre-existing
alternates/parity-floor discrepancy, not an audit regression. Investigate its
behavior and retained floor evidence separately; **do not lower the floor to
make this result green**. All other selected scripts meet or exceed their
floors in both builds.

Local logs and CSVs are under `/home/scratch/sley-audit-parity/`; build/test/lint
logs use `/home/scratch/sley-audit-*.log`. The full 891-script sweep and other
platform/hash lanes were not run, and neither consumer workspace was rebuilt.
The consumer analysis is source/manifest evidence at the revisions above.
