# Git Parity and Performance Checklist

Goal: make sley a pure-Rust, minimal-dependency, drop-in-compatible Git
implementation for the core workflows that matter to users and downstream
libraries, while matching or beating upstream git performance on representative
repositories.

Compatibility target: upstream Git behavior as exercised by the local system git,
the upstream `t/*.sh` suite, and repo-specific parity tests. The existing tracker
pins Git 2.54.0 as the current upstream target; refresh this pin whenever the
upstream harness is regenerated.

Non-goals:

- Do not port C Git internals directly.
- Do not depend on libgit2 or gitoxide for Git semantics.
- Do not add broad dependencies where a small first-party Rust primitive is
  enough.
- Do not accept user-visible incompatibilities without documenting them here.

## Completion Definition

- [ ] Upstream parity harness is regenerated against the current branch.
- [ ] Every high-impact upstream failure is categorized in this file.
- [ ] Core user workflows are byte-compatible or behavior-compatible with git:
  `init`, `config`, `add`, `status`, `commit`, `log`, `rev-list`, `diff`,
  `checkout`, `switch`, `restore`, `branch`, `tag`, `merge`, `stash`, `clone`,
  `fetch`, `push`, `gc`, `repack`, `cat-file`, `hash-object`, `ls-tree`,
  `ls-files`, `rev-parse`, `show-ref`, `update-ref`, and `update-index`.
- [ ] Core plumbing formats round-trip through upstream git: loose objects,
  index v2/v3/v4, packs, pack indexes, commit-graph, MIDX, bundles, refs,
  packed-refs, reflogs, and reftable where supported.
- [ ] Core transport workflows interoperate with upstream git over local, SSH,
  and HTTP(S): clone, fetch, push, upload-pack, receive-pack, shallow fetch, and
  partial/promisor pack handling.
- [ ] Performance benchmarks show sley at git parity or better for:
  `clone`, `fetch`, `push`, `repack`, `cat-file --batch`,
  `cat-file --batch-check`, `status`, `diff`, `log --format=%H`,
  `rev-list --count`, and `merge-base`.
- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets --no-deps --
  -D warnings`, and `cargo test --workspace` pass.

## Verified Minimal Parity (main)

This section records what is already covered by `PARITY.md`, upstream byte-compare
integration tests, and the regression bundles on `main`. It does **not** mean full
upstream `t/*.sh` parity or complete option matrices — see Phase 5+ for depth
gaps.

- [x] **Workflow sequencer:** `merge` (FF, 3-way, conflict, `--abort`,
  `--continue`), `pull` (FF, merge, `pull.rebase`, conflict abort/continue,
  rebase conflict continue), `rebase` (clean, conflict, `--abort`/`--continue`/
  `--skip`), `cherry-pick`, `revert`, and `commit` during in-progress
  merge/rebase — see `crates/sley-cli/tests/{merge,pull*,rebase*,sequencer,
  commit_merge,commit_rebase}.rs`.
- [x] **Transport:** local / SSH / HTTP(S) `clone`, `fetch`, `push`, `ls-remote`;
  protocol v2 HTTP `ls-refs` ref advertisements; shallow `--depth` over HTTP —
  see `crates/sley-cli/tests/{clone,http,push,ls_remote}.rs` and
  `crates/sley-remote`.
- [x] **Hygiene:** `gc`, `repack`, `fsck`, `apply`, `maintenance run` (gc
  delegation) — see `crates/sley-cli/tests/{maintenance,fsck}.rs`.
- [x] **Conflict replay:** `rerere status` / `clear` / `forget` — see
  `crates/sley-cli/tests/rerere.rs`.
- [x] **Plumbing (bcc2bea+):** `verify-pack`, `show-index`, `unpack-file`,
  `prune`, `prune-packed`, `update-server-info`, `check-ref-format`,
  `stripspace`, `var`, `check-mailmap`, `replace`, `get-tar-commit-id` — see
  matching `crates/sley-cli/tests/*.rs` suites.
- [x] **Config (partial):** modern + legacy local/file/stdin forms, typed reads,
  `--default`, `--get-urlmatch` common cases — see `crates/sley-cli/tests/config.rs`.
- [ ] **Still open at completion-definition level:** upstream `t/*.sh` harness
  refresh, reftable, hooks/GPG, interactive rebase, full shallow
  (`--shallow-since`/`--shallow-exclude`/unshallow), promisor/partial clone
  filters, merge/pull diffstat output, and performance benchmark gates.

Authoritative feature bullets live in `PARITY.md` (Implemented Initial Surface).
This checklist tracks depth, harness, and release gates.

## Phase 0: Refresh the Truth Source

- [ ] Re-run the upstream Git `t/*.sh` compatibility harness.
- [ ] Capture exact upstream git version, sley commit, platform, filesystem, and
  hash algorithm.
- [ ] Regenerate or update `crates/sley-testkit/upstream-gap-map.txt`.
- [ ] Split results into:
  - [ ] Already fixed stale gaps.
  - [ ] Correctness failures.
  - [ ] Output or exit-code mismatches.
  - [ ] Unsupported command/option surfaces.
  - [ ] Harness-only gaps or dependency commands.
  - [ ] Performance regressions.
- [ ] Add a small regression test before fixing each newly confirmed gap.
- [ ] Add benchmark fixtures for every command considered performance-critical.

## Phase 1: CLI Contract Parity

Git compatibility often fails at the command boundary even when the underlying
Rust libraries are correct. This phase makes the CLI behave like git before
deeper format work.

- [ ] Normalize usage errors to git exit code `129` where upstream expects it.
- [ ] Audit every command that returns `GitError::Command` for usage-style
  errors and convert true usage errors to `GitError::Exit(129)`.
- [ ] Preserve fatal/runtime failures as non-usage exits.
- [ ] Add shared helpers for:
  - [ ] Unknown option.
  - [ ] Option missing value.
  - [ ] Option takes no value.
  - [ ] Mutually exclusive modes.
  - [ ] Too many or too few operands.
  - [ ] Usage text printing.
- [ ] Add tests for representative commands:
  - [ ] `cat-file`.
  - [x] `ls-tree` usage/option exit-status parity.
  - [ ] `config`.
  - [ ] `hash-object`.
  - [ ] `stash`.
  - [ ] `branch`.
  - [ ] `remote`.
  - [ ] `rev-parse`.
  - [ ] `status`.
- [ ] Implement alias expansion from config before command dispatch.
- [ ] Ensure aliases honor `-c` config overrides.
- [ ] Match git alias recursion and failure behavior.
- [ ] Preserve global option ordering semantics around aliases.
- [ ] Add tests for simple aliases, shell aliases, alias recursion, aliases with
  global `-c`, and unsupported aliased commands.

## Phase 2: Config Parity

Config is a major multiplier because many upstream tests and real workflows use
it for setup.

- [ ] Implement modern `git config` subcommands:
  - [x] `git config list` basic local form.
  - [x] `git config get` basic local form, including `--all`, `--bool`, and
    `--default`.
  - [x] `git config set` basic local form, including `--append`.
  - [x] `git config unset` basic local form, including `--all`.
  - [x] `git config rename-section` basic local form.
  - [x] `git config remove-section` basic local form.
- [ ] Preserve legacy forms:
  - [x] `--list`.
  - [x] `--get`.
  - [x] `--get-all`.
  - [x] `--get-regexp`.
  - [x] `--add`.
  - [x] `--replace-all`.
  - [x] `--unset`.
  - [x] `--unset-all`.
- [x] Preserve written section and variable case where git preserves it.
- [ ] Preserve or intentionally rewrite comments according to git behavior.
- [x] Support `--comment` for write operations.
- [x] Support positional value-pattern matching for replace/unset flows.
- [x] Support `--file <path>` for read and write actions.
- [x] Support `--file -` for read-only stdin actions and reject writes like
  git.
- [x] Support `--show-origin`, `--show-scope`, and `--name-only` combinations
  for single-source local, file, and stdin config reads.
- [ ] Track per-entry origins through include/includeIf expansion for
  `--show-origin`.
- [ ] Complete typed value handling:
  - [x] `--bool`.
  - [x] `--int`.
  - [x] `--bool-or-int`.
  - [x] `--path`.
  - [ ] `--expiry-date`.
    - [x] Deterministic numeric epoch, `now`, and `never` values.
    - [ ] Full approxidate/natural-language parsing.
  - [x] `--type=bool`.
  - [x] `--type=int`.
  - [x] `--type=bool-or-int`.
  - [x] `--type=path`.
  - [ ] `--type=expiry-date`.
    - [x] Deterministic numeric epoch, `now`, and `never` values.
    - [ ] Full approxidate/natural-language parsing.
  - [ ] `--type=color`.
    - [x] Named foreground/background colors, common attributes, ANSI 0-255
      indexed colors, `#rrggbb`, and `normal`.
    - [x] Legacy `--get-color <name> [<default>]`.
    - [x] Legacy `--get-colorbool <name> [<stdout-is-tty>]`.
    - [ ] Full color grammar and exact invalid diagnostics for all failure
      paths.
- [x] Implement `--default` git semantics.
- [ ] Implement URL matching behavior for `urlmatch`.
  - [x] Basic `--get-urlmatch` for base sections, longest raw URL-prefix
    subsection matches, specific-key reads, all-key reads, and `-z` output.
  - [x] Common URL canonicalization for scheme/host case, default HTTP(S)
    ports, and slash-tolerant path-prefix matching.
  - [x] User-specific URL section matching and generic fallback for user URLs.
  - [x] Percent-escaped path matching for non-slash bytes, while keeping
    encoded slash distinct from a path separator.
  - [x] Bracketed IPv6 hosts with case-insensitive address text and port
    matching.
  - [ ] Full Git URL canonicalization and edge-case precedence semantics
    for less-common URL forms.
- [ ] Ensure include and includeIf behavior remains byte-compatible.
- [ ] Add regression tests for mixed-case sections, multi-value keys, comments,
  stdin config, invalid config, and legacy/subcommand equivalence.

## Phase 3: Revision Grammar and Object Name Resolution

Many commands share revision parsing. The right fix is a Rust-native revision
engine with command-facing options rather than ad hoc CLI parsing.

- [ ] Complete `<rev>:<path>` lookup for blobs, trees, and submodules.
- [ ] Support `:<stage>:<path>` index-stage lookup where git accepts it.
- [ ] Support peel syntax:
  - [ ] `<rev>^{}`.
  - [ ] `<rev>^{commit}`.
  - [ ] `<rev>^{tree}`.
  - [ ] `<rev>^{tag}`.
  - [ ] `<rev>^{object}`.
- [ ] Support commit-message search syntax:
  - [ ] `:/pattern`.
  - [ ] `HEAD^{/pattern}`.
  - [ ] Search ordering and tie-breaking matching git.
- [ ] Support reflog date selectors:
  - [ ] `@{yesterday}`.
  - [ ] `@{<date>}`.
  - [ ] `<branch>@{<date>}`.
  - [ ] Time zone handling and ambiguous selectors.
- [ ] Support upstream/push selectors comprehensively:
  - [ ] `@{u}` / `@{upstream}`.
  - [ ] `@{push}`.
  - [ ] Error messages for missing branch config.
- [ ] Support revision-limiting pass-through in `rev-parse`:
  - [ ] `--since`.
  - [ ] `--after`.
  - [ ] `--until`.
  - [ ] `--before`.
  - [ ] `--max-age`.
  - [ ] `--min-age`.
- [ ] Keep abbreviation behavior terminating and clamped for SHA-1 and SHA-256.
- [ ] Add shared tests that exercise the same revision expressions through
  `rev-parse`, `cat-file`, `ls-tree`, `show`, `log`, and `diff`.

## Phase 4: Object, Pack, and Plumbing Surface

The object database is strong, but a drop-in git needs more command and format
surface around it.

- [ ] Implement or expose `pack-objects`.
- [ ] Implement or expose `index-pack`.
- [ ] Implement or expose `unpack-objects`.
- [x] Implement or expose `verify-pack`.
- [x] Implement `show-index`.
- [x] Implement `unpack-file`.
- [x] Implement `prune`.
- [x] Implement `prune-packed`.
- [x] Implement `update-server-info`.
- [x] Implement `check-ref-format`.
- [x] Implement `stripspace`.
- [x] Implement `var`.
- [x] Implement `check-mailmap`.
- [x] Implement `replace`.
- [ ] Implement `fast-import`.
- [ ] Implement `fast-export`.
- [x] Implement `get-tar-commit-id`.
- [ ] Ensure pack generation supports:
  - [ ] Thin packs.
  - [ ] OFS deltas.
  - [ ] REF deltas.
  - [ ] Delta depth/window controls.
  - [ ] Pack reuse where safe.
  - [ ] Bitmap-aware selection.
  - [ ] Cruft packs.
  - [ ] Promisor packs.
  - [ ] SHA-256 repositories.
- [ ] Ensure pack validation supports corrupt loose/packed objects, broken
  deltas, dictionary zlib failures, missing bases, and strict fsck modes.

## Phase 5: Command Depth for Implemented Porcelain

These commands exist, but their option and semantic matrices are still narrower
than git.

- [ ] `cat-file`
  - [ ] Legacy `<type> <object>` form.
  - [ ] Batch object arguments.
  - [ ] `--batch-command`.
  - [ ] `-z` / `-Z` batch variants.
  - [ ] `%(objectmode)`.
  - [ ] `%(deltabase)`.
  - [ ] `%(rest)`.
  - [ ] `--follow-symlinks`.
  - [ ] Broken-object behavior for `-e`, `-t`, and `-s`.
- [ ] `hash-object`
  - [ ] Mutually exclusive flag validation.
  - [ ] Filter behavior for CRLF and attributes.
  - [ ] Structure validation for `tree`, `commit`, and `tag`.
  - [ ] Exact error wording for invalid `-t` values.
- [ ] `ls-files`
  - [ ] Symlink handling in `--others`.
  - [ ] Embedded `.git` file/directory handling.
  - [ ] `--directory` pathspec edge cases.
  - [ ] More exclude/pathspec combinations.
- [ ] `ls-tree`
  - [x] Complete currently known incompatible option detection for
    `--name-only` / `--name-status`, format-altering options with `--format`,
    and object/name/long mode conflicts.
  - [x] Return git-style usage exit `129` for missing tree-ish, missing
    `--format` value, unknown options, invalid `--abbrev=<n>`, and covered
    incompatible options.
  - [ ] Broken tree behavior.
  - [ ] Full format placeholder matrix.
- [ ] `checkout` / `switch` / `restore`
  - [ ] Clean-tree branch creation edge cases.
  - [ ] `--orphan`.
  - [ ] Path checkout modes.
  - [ ] Conflict and unmerged index behavior.
  - [ ] Sparse checkout interactions.
- [ ] `commit`
  - [ ] Pathspec commits.
  - [ ] Interactive patch selection.
  - [ ] Signing options.
  - [ ] Cleanup modes not yet covered.
  - [ ] Hook behavior.
- [ ] `tag`
  - [ ] Signed tag creation.
  - [ ] Signed tag verification.
  - [ ] Full listing format/sort/filter surface.
- [ ] `verify-commit` / `verify-tag`
  - [ ] Actual signature verification.
  - [ ] SSH, GPG, and X.509 marker behavior.
  - [ ] Trust-model-compatible error messages.
- [ ] `describe`
  - [ ] `--contains`.
  - [ ] More candidate/tiebreak edge cases.
- [ ] `stash`
  - [ ] `stash push --patch`.
  - [ ] `stash save --patch`.
  - [ ] `stash list` revisions/pathspecs.
  - [ ] Remaining show/push/save option surface.
- [ ] `bisect`
  - [ ] `bisect run`.
  - [ ] `bisect visualize`.
- [ ] `fsck`
  - [ ] Strict modes.
  - [ ] Message IDs and severities.
  - [ ] Promisor-aware checks.
  - [ ] Broken packed-object scenarios.
- [ ] `archive`
  - [ ] Additional archive formats if required.
  - [ ] Remote archive behavior.
- [ ] `log` / `rev-list`
  - [ ] Early-stop priority queue for `-n`.
  - [ ] Full history simplification.
  - [ ] More date ordering and tie-breaking parity.
  - [ ] Object filters matching git exactly.
  - [ ] Bitmap acceleration where available.
- [ ] `diff`
  - [ ] Remaining diff algorithms and options.
  - [ ] Full rename/copy heuristics.
  - [ ] Word diff and color-moved parity.
  - [ ] Submodule diff formats.
  - [ ] Binary patch compatibility.
- [x] `merge` (minimal)
  - [x] Fast-forward and three-way merge with conflict markers.
  - [x] `--abort` and `--continue` after conflict resolution.
  - [ ] Post-merge diffstat output (upstream prints stat summary).
  - [ ] Strategy selection, octopus, and broader option surface.
- [x] `pull` (minimal)
  - [x] Fetch + FF merge, three-way merge, and `pull.rebase` replay.
  - [x] Conflicted pull abort/continue (merge path) and rebase-conflict continue.
  - [ ] Broader flag surface (`--autostash`, `--verify-signatures`, etc.).
- [x] `rebase` (minimal)
  - [x] Onto upstream replay with `rebase-merge` state files.
  - [x] `--abort`, `--continue`, and `--skip` on covered fixtures.
  - [ ] Interactive rebase, autostash, merge/rebase-merges modes.
- [x] `cherry-pick` / `revert` (minimal)
  - [x] Clean and conflict paths with `--abort` / `--continue`.
- [x] `commit` during sequencer (minimal)
  - [x] Conclude in-progress merge/rebase without `--continue`.
- [x] `gc` / `repack` / `fsck` / `apply` (minimal)
  - [x] Covered small-repo workflows with upstream interop tests.
- [x] `maintenance` (minimal)
  - [x] `maintenance run` delegates to gc for covered cases.
  - [ ] `maintenance start` / `stop` / register / scheduled tasks.
- [x] `rerere` (minimal)
  - [x] `status`, `clear`, `forget` on covered empty/disabled cases.

## Phase 6: Refs, Reflogs, and Repository Layout

- [ ] Support pseudo-ref and top-level symref names the way git does.
- [ ] Match git d/f conflict errors for loose refs and symbolic refs.
- [ ] Implement graceful ENOTDIR/EISDIR behavior in ref resolution.
- [ ] Complete packed-refs edge cases and peeled tag behavior.
- [ ] Complete reftable backend support:
  - [ ] Init selection.
  - [ ] Repository extension validation.
  - [ ] Read/write transactions.
  - [ ] Reflog behavior.
- [ ] Complete `init` parity:
  - [ ] `--template`.
  - [ ] `init.templatedir`.
  - [ ] `~` expansion.
  - [ ] `--separate-git-dir`.
  - [ ] Re-init movement behavior.
  - [ ] `--ref-format=reftable`.
  - [ ] `init.defaultRefFormat`.
  - [ ] `GIT_DEFAULT_REF_FORMAT`.
  - [ ] `GIT_DEFAULT_HASH`.
  - [ ] `init.defaultObjectFormat`.
  - [ ] `init.defaultBranch` advice.
  - [ ] `--shared`.
  - [ ] `core.sharedRepository`.
- [ ] Ensure repository discovery honors `core.bare`, linked worktrees,
  commondir, gitfiles, and environment variables in git-compatible order.

## Phase 7: Worktree, Attributes, and Filters

- [ ] Fold attribute loading into single worktree traversal where possible.
- [ ] Use cache-tree/index information to avoid unnecessary HEAD tree reads.
- [ ] Confirm racy-git stat-cache behavior against git-written indexes.
- [ ] Complete clean/smudge filters:
  - [ ] Add.
  - [ ] Checkout.
  - [ ] Restore.
  - [ ] Reset hard.
  - [ ] Stash apply/pop.
- [ ] Complete EOL normalization edge cases.
- [ ] Complete executable-bit and symlink behavior across platforms.
- [ ] Complete sparse checkout and sparse index parity.
- [ ] Complete pathspec grammar:
  - [ ] Magic prefixes.
  - [ ] Glob matching.
  - [ ] Attr pathspecs.
  - [ ] Exclude pathspecs.
  - [ ] Case-insensitive behavior where configured.

## Phase 8: Transport and Server Parity

- [x] Protocol v2 HTTP ref advertisements via `ls-refs` RPC (fetch/clone/push
  still use covered v0/v1 + v2 fetch paths where implemented).
- [x] Protocol v0/v1 compatibility for upstream git servers (HTTP/SSH/local interop
  tests).
- [ ] Complete shallow clone/fetch:
  - [x] `--depth` over HTTP (covered interop test).
  - [ ] `--shallow-since`.
  - [ ] `--shallow-exclude`.
  - [ ] Unshallow.
  - [x] Deepen (library + HTTP shallow fetch plumbing; broaden CLI coverage).
- [ ] Complete partial clone filters:
  - [ ] `blob:none`.
  - [ ] `blob:limit`.
  - [ ] `tree:<depth>`.
  - [ ] Promisor fetch-on-demand.
- [ ] Complete push negotiation:
  - [ ] Atomic push.
  - [ ] Push options.
  - [ ] Signed push if required.
  - [ ] Force-with-lease.
  - [ ] Delete and refspec edge cases.
- [ ] Implement or expose transport helper commands where needed:
  - [ ] `fetch-pack`.
  - [ ] `send-pack`.
  - [ ] `remote-http`.
  - [ ] `remote-https`.
  - [ ] `remote-ext`.
  - [ ] `remote-fd`.
  - [ ] `http-backend`.
  - [ ] `daemon`.
  - [ ] `shell`.
- [ ] Forward TLS backend selection cleanly:
  - [ ] rustls.
  - [ ] native-tls.
  - [ ] platform verifier.
- [ ] Complete credential helper integration:
  - [ ] `credential`.
  - [ ] `credential-store`.
  - [ ] `credential-cache`.
  - [ ] Platform helpers where practical.

## Phase 9: Library Architecture and Rust Ergonomics

- [ ] Continue decomposing `sley-cli` into crate-owned logic.
- [ ] Keep `sley-cli` as dispatch, argument parsing, and output formatting only.
- [ ] Move reusable command semantics into domain crates:
  - [ ] `sley-config`.
  - [ ] `sley-rev`.
  - [ ] `sley-worktree`.
  - [ ] `sley-diff-merge`.
  - [ ] `sley-sequencer`.
  - [ ] `sley-remote`.
  - [ ] `sley-pack`.
  - [ ] `sley-odb`.
  - [ ] `sley-refs`.
- [ ] Replace stringly APIs with lossless newtypes:
  - [ ] Full ref names.
  - [ ] Short ref names.
  - [ ] Pathspecs.
  - [ ] Repo-relative paths.
  - [ ] Config keys.
  - [ ] Remote names.
  - [ ] Reflog selectors.
- [ ] Keep borrowed parse views for large byte buffers.
- [ ] Avoid object clones in hot paths.
- [ ] Prefer iterators and streaming writers over materializing full output.
- [ ] Use `Cow` only where mutation is rare and ergonomics are worth it.
- [ ] Keep trust boundaries explicit:
  - [ ] Trusted generated packs.
  - [ ] Untrusted raw packs.
  - [ ] Verified reads.
  - [ ] fsck validation.
- [ ] Keep unsafe isolated in audited crates only.

## Phase 10: Performance Parity

Benchmarks must compare against upstream git on the same machine and fixture.
Every optimization must preserve compatibility tests.

- [ ] Maintain benchmark fixtures:
  - [ ] Small loose repo.
  - [ ] Small packed repo.
  - [ ] Large real repo.
  - [ ] Many-ref repo.
  - [ ] Large binary-object repo.
  - [ ] Sparse checkout repo.
  - [ ] Shallow/partial clone repo.
- [ ] Track benchmark commands:
  - [ ] `clone`.
  - [ ] `fetch`.
  - [ ] `push`.
  - [ ] `repack`.
  - [ ] `gc`.
  - [ ] `cat-file --batch`.
  - [ ] `cat-file --batch-check`.
  - [ ] `status`.
  - [ ] `diff HEAD~1 HEAD`.
  - [ ] `diff --cached`.
  - [ ] `log --format=%H`.
  - [ ] `rev-list --count`.
  - [ ] `merge-base`.
  - [ ] `ls-files`.
  - [ ] `checkout`.
  - [ ] `read-tree`.
- [ ] For each command, record:
  - [ ] Mean time.
  - [ ] Standard deviation.
  - [ ] Cold-cache behavior where relevant.
  - [ ] Warm-cache behavior.
  - [ ] Object count.
  - [ ] Pack size.
  - [ ] Ref count.
  - [ ] Commit count.
- [ ] Known performance work:
  - [ ] Route `log --format=%H` through graph-backed metadata walks.
  - [ ] Add early-stop traversal for `-n`.
  - [ ] Use bitmaps for object enumeration where available.
  - [ ] Avoid per-object CLI revision resolution in batch paths.
  - [ ] Avoid redundant worktree walks in status.
  - [ ] Avoid redundant HEAD tree reads when cache-tree is valid.
  - [ ] Stream pack writes to disk when large packs would otherwise double
    memory.
  - [ ] Reuse compression/decompression state safely.
  - [ ] Keep generated-pack direct install fast paths for trusted in-process
    paths.
  - [ ] Use mmap pack reads behind the existing feature for large repositories.

## Phase 11: Release Gate

- [ ] Full workspace tests pass.
- [ ] Upstream selected `t/*.sh` parity pass rate improves or documented gaps
  shrink.
- [ ] No new upstream parity regressions.
- [ ] No benchmark regression larger than 5% without explicit justification.
- [ ] New public APIs are documented.
- [ ] New trust boundaries are documented.
- [ ] New command behavior has direct tests against system git.
- [ ] Push branch and publish summary with:
  - [ ] Compatibility changes.
  - [ ] Performance changes.
  - [ ] Remaining gaps.
  - [ ] Tests run.

## Next Recommended Slices

1. Refresh upstream harness results and delete stale gaps from the old gap map.
2. Normalize usage exit code `129` across `cat-file`, `ls-tree`, `hash-object`,
   and `config` (fix `--get-colorbool` regression in `config.rs`).
3. Merge/pull post-merge diffstat output to match upstream stdout.
4. Implement alias expansion before command dispatch.
5. Finish `<rev>:<path>` and commit-message search revision grammar.
6. Fill `cat-file` batch/legacy format gaps.
7. Route `log --format=%H` through graph-backed walks and add `-n` early-stop.
8. Reftable init/read/write and broader shallow (`--shallow-since` / unshallow).

## Progress Log

- 2026-06-07: Added this checklist and created the active Codex goal for Git
  parity plus performance parity.
- 2026-06-07: Attempted Phase 0 upstream harness inspection. The runner exists
  at `crates/sley-testkit/scripts/run-upstream-tests.sh`, but this environment
  has no `SLEY_UPSTREAM_T` or `GIT_SRC_DIR` configured, so a real upstream
  refresh cannot run here yet.
- 2026-06-07: Started Phase 1 with `ls-tree` usage parity. `cmd_ls_tree` now
  exits `129` for covered usage errors, and
  `crates/sley-cli/tests/ls_tree.rs` compares those exit statuses against
  system git.
- 2026-06-07: Added Phase 2 coverage for basic local modern `git config`
  subcommands. `cmd_config` now normalizes `list`, `get`, `set`, and `unset`
  into the existing Rust-native actions, including `get --all`, `set
  --append`, and `unset --all`; `crates/sley-cli/tests/config.rs` compares the
  behavior against system git.
- 2026-06-07: Completed the basic modern `git config` subcommand dispatch set by
  adding `rename-section` and `remove-section`, then added file-backed config
  sources. `cmd_config` now supports `--file <path>` outside a repository for
  reads and writes, supports `--file -` for stdin-backed reads, rejects
  stdin-backed writes with git's `128` behavior, and tests compare these paths
  against system git.
- 2026-06-07: Preserved config section and variable spelling through
  `sley-config` parsing and CLI writes while keeping lookups case-insensitive.
  Added byte-for-byte upstream-git writer tests for mixed-case file-backed
  config edits and section renames. Comment-preserving rewrites remain a
  separate document-model/trivia task.
- 2026-06-07: Wired `git config` typed reads to shared `sley-config` primitives:
  `--bool`, `--int`, `--bool-or-int`, `--path`, and their `--type=` forms now
  match upstream for covered booleans, hex/octal/unit integers, boolean-or-int
  normalization, and `~` path expansion. `patch-id` and `update-ref` config
  boolean checks now use the same shared parser.
- 2026-06-07: Added `git config --show-origin` and `--show-scope` display
  metadata for local, `--file <path>`, and stdin-backed reads, including `-z`
  output. The CLI now shares a single metadata writer for `get`, `get-all`,
  `get-regexp`, and `list`; per-entry include origins remain open.
- 2026-06-07: Added value-pattern support for `git config --replace-all`,
  `--unset`, and `--unset-all`. The implementation reuses the CLI's existing
  dependency-free config regex matcher, appends on replace no-match like git,
  removes all matching values for `--unset-all`, and requires exactly one match
  for patterned `--unset`.
- 2026-06-07: Extended config value-pattern matching with `--fixed-value` so
  replace/unset flows can choose exact string matching instead of regex-like
  matching, again compared directly against upstream git.
- 2026-06-07: Added `git config --comment` for set/add/replace writes. Config
  entries now carry optional inline comments through the Rust document model,
  canonical writes emit ` # <comment>`, multiline comments fail with git's
  `128` behavior, and replacing an existing key preserves its entry position.
- 2026-06-07: Added the deterministic part of `git config --expiry-date` and
  `--type=expiry-date` typed reads: numeric epoch values, `now`, and `never`
  now normalize like upstream git, including typed `--default` values. Broader
  approxidate parsing remains open. Also made `--get-all` stream borrowed
  config values instead of cloning them into a temporary vector.
- 2026-06-07: Added dependency-free `git config --type=color` formatting for
  the common Git color grammar: named colors, foreground/background pairs,
  attributes, 0-255 indexed colors, `#rrggbb`, `normal`, and typed defaults.
  Full legacy `--get-color*` behavior and exact invalid diagnostics remain open.
- 2026-06-07: Added legacy `git config --get-color <name> [<default>]` on top
  of the same dependency-free color formatter, including no-newline output,
  missing-key success with no output, positional defaults, and legacy invalid
  default exit behavior.
- 2026-06-07: Added legacy `git config --get-colorbool <name>
  [<stdout-is-tty>]` behavior for the covered upstream modes: status-only
  one-argument calls, printed two-argument calls, `always` handling, missing
  keys, invalid config values, invalid command-line TTY hints, and metadata
  option rejection.
- 2026-06-07: Added basic `git config --get-urlmatch` support over the
  dependency-free config model: base section values, longest matching URL
  subsection per key, specific-key reads, all-key reads, `-z` output, missing
  matches, and metadata/name-only rejection are now covered against upstream
  git. Full URL canonicalization remains open.
- 2026-06-07: Tightened `--get-urlmatch` from raw prefix matching to a small
  parsed URL matcher for the common Git canonicalization cases: scheme and host
  case-insensitivity, default HTTP(S) port equivalence, non-default port
  separation, and slash-tolerant path-prefix matching. IPv6, percent escapes,
  and user-specific URL edge cases remain open.
- 2026-06-07: Extended the parsed `--get-urlmatch` matcher with user-specific
  URL sections plus generic fallback for user URLs, and percent-decoded path
  matching for non-slash escapes while keeping `%2f` distinct from `/`.
- 2026-06-07: Added bracketed IPv6 host support to `--get-urlmatch`, including
  case-insensitive address text, default HTTPS port equivalence, and
  non-default port separation.
- 2026-06-07: Completed covered `git config --default` semantics for normal
  and typed `get` reads, including Git-style `129` usage errors for missing
  default values and invalid action combinations.
- 2026-06-07: Added `git check-ref-format` plumbing coverage for ordinary ref
  validation, `--allow-onelevel`, `--normalize`/`--print`,
  `--refspec-pattern`, basic `--branch`, and usage errors, all compared against
  upstream git.
- 2026-06-07: Added standalone `git stripspace` plumbing support, reusing the
  shared Rust stripspace helper for whitespace/comment stripping and adding
  `--comment-lines`, mutual-exclusion errors, usage output, and stdin/stdout
  behavior compared against upstream git.
- 2026-06-08: Merged codex-branch plumbing (`bcc2bea`) onto `main`, then layered
  protocol v2 HTTP `ls-refs`, `maintenance run`, and `rerere` parity slices.
- 2026-06-08: Ported `git pull` and `git rebase` from the `cd16d98` `git-cli`
  lineage onto `sley-cli`, including `merge --continue`, in-progress
  merge/rebase `commit`, and 29 new upstream interop tests (`574fc26`).
- 2026-06-08: Added **Verified Minimal Parity** section to this checklist and
  reconciled stale gap wording in `PARITY.md` Major Gaps against implemented
  transport/sequencer coverage.
