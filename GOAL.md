  # Rust-First Git Parity Plan

  ## Summary

  Create a Codex goal with objective: “Build a pure-Rust, minimal-dependency, drop-in-compatible Git replacement tracking latest stable upstream Git, currently Git 2.55.0, with complete format, protocol, plumbing, and porcelain parity.”

  Use a greenfield Rust workspace. Do not port C Git or depend on libgit2/gitoxide for implementation; use upstream Git docs/tests and gitoxide’s crate decomposition only as research input. Keep Git compatibility at the boundaries, but design
  internals around typed Rust APIs, streaming I/O, transactional filesystem updates, explicit trust/security policy, and testable crate boundaries.

  ## Workspace Slices

  - git-core: object IDs, hash abstraction, errors, byte/path types, time/signature types, capabilities, compatibility feature flags.
  - git-formats: parsers/writers for repository layout, config, index, pack/idx/rev/mtimes, MIDX, commit-graph, bundle, reftable, signatures.
  - git-odb: loose objects, pack reading, delta resolution, pack indexing, object validation, alternates, promisor/partial clone support.
  - git-refs: loose refs, packed refs, reflogs, symbolic refs, transactions, reftable backend.
  - git-rev: revision syntax, refname resolution, graph traversal, reachability, commit-graph acceleration.
  - git-worktree: checkout, status, sparse checkout/index, ignore, attributes, filters, clean/smudge, EOL/filemode/symlink behavior.
  - git-diff-merge: diff algorithms, rename/copy detection, apply, merge strategies, conflict representation, rerere.
  - git-sequencer: commit, cherry-pick, revert, rebase, bisect, stash, notes, history-editing workflows including new Git 2.54 git history surface.
  - git-pack: pack generation, delta selection, thin packs, bitmaps, cruft packs, geometric repacking, fsck/repair hooks.
  - git-transport: pkt-line, protocol v0/v1/v2, upload-pack, receive-pack, fetch, push, clone, HTTP(S), SSH command transport, auth/credentials.
  - git-cli: git binary compatible command dispatch, porcelain/plumbing output compatibility, config/env precedence, exit codes.
  - git-testkit: upstream Git fixture importer, golden-output runner, fuzz targets, interop harness, large-repo benchmarks.

  ## Implementation Strategy

  - Start with public contracts: every crate exposes typed parse/write/validate APIs plus streaming variants for large repositories; CLI code depends on crate APIs, never on file-format shortcuts.
  - Establish a compatibility harness before feature work: run upstream git and Rust git against the same temp repos, compare objects, refs, index bytes where required, command output where user-visible, and exit/status behavior.
  - Build storage first: repository discovery/config, object model, loose objects, index v2/v3/v4, pack read/write/index, refs/reflogs, commit traversal.
  - Then build user workflows: init, hash-object, cat-file, ls-tree, update-index, status, add, commit, log, diff, checkout/switch/restore, branch/tag, merge, rebase, stash.
  - Then complete network/server parity: clone/fetch/push over local, SSH, and HTTP(S), protocol v2 first with v0/v1 compatibility, shallow/partial/filter negotiation.
  - Then finish advanced parity: submodules, worktrees, sparse index, LFS pointer-aware behavior where Git expects hooks/filters, maintenance/gc, bundles, fsck, mail/apply/am, bisect, notes, archive, daemon/http-backend/git-shell equivalents.
  - Keep dependencies pragmatic but generic only: allow crates for compression, hashes, TLS/HTTP, SSH process transport, CLI parsing, mmap, tempfiles, locking, fuzzing, and test infrastructure. Git semantics stay first-party.

  ## Test Plan

  - Golden parity: import and run relevant upstream Git test scripts in phases, mapping unsupported tests to tracked parity gaps.
  - Format conformance: round-trip official binary/text formats and compare byte-for-byte when Git requires canonical output.
  - Interop: create repos with C Git and operate with Rust Git, then reverse; include SHA-1 and SHA-256 repos.
  - Protocol: fetch/push/clone against upstream Git server, GitHub-compatible HTTP(S), local file remotes, and SSH command remotes.
  - Robustness: fuzz parsers for config, index, pack, pkt-line, refs, revision specs, attributes, ignore, and pathspecs.
  - Scale: benchmark Linux/kernel-sized repos, large monorepos, many-ref repos, large packs, sparse checkout, partial clone, and Windows/macOS path edge cases.

  ## Assumptions

  - Compatibility target is latest stable upstream Git, currently 2.55.0 released April 20, 2026.
  - Public behavior must match Git even where internals are cleaner; deliberate incompatibilities require an explicit tracked decision.
  - Implementation must be pure Rust, but may use minimal audited Rust dependencies for non-Git primitives.
  - The first deliverable is a workspace plus conformance harness and storage-core milestone, not a thin CLI prototype.

  ## Research Sources

  - Git latest release/version: https://git-scm.com/install/windows.html
  - Git technical docs index: https://git-scm.com/docs/git
  - Pack format: https://www.kernel.org/pub/software/scm/git/docs/gitformat-pack.html
  - Index format: https://www.kernel.org/pub/software/scm/git/docs/gitformat-index.html
  - Repository layout/versioning: https://www.kernel.org/pub/software/scm/git/docs/gitrepository-layout.html
  - Reftable rationale/format: https://git-scm.com/docs/reftable
  - Gitoxide crate decomposition reference: https://github.com/GitoxideLabs/gitoxide
