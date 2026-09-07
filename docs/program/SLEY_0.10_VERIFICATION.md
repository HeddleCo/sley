# Sley 0.10 embedding cut verification

Implementation verified: `a0721ab33195d7f5fb95baf9b431986f882d5cbd`.
The following evidence was collected on Linux/x86_64 with Git 2.55.0, using
`CARGO_TARGET_DIR=/home/scratch/sley-cuts-target` and `TMPDIR=/home/scratch`.
The [migration guide](../SLEY_0.10_MIGRATION.md) lists the concrete removed
symbols, changed signatures/features, and consumer replacements.

## Finished workspace run

Every command completed successfully; build, test, lint, documentation, and
release logs contain zero warnings. The full workspace suite ran **3,670 passing
tests, zero failures, two ignored**, across 212 targets including doctests.

```text
cargo build --workspace
PASS — Finished dev profile, 10.43s
cargo test --workspace
PASS — 3670 passed; 0 failed; 2 ignored
cargo clippy --all-targets -- -D warnings
PASS
cargo clippy --locked --workspace --all-targets --all-features --no-deps -- -D warnings
PASS — Finished dev profile, 22.44s
cargo clippy --locked -p sley-remote --all-targets --no-default-features --no-deps -- -D warnings
PASS — Finished dev profile, 6.40s
cargo check --locked -p sley --no-default-features
PASS
python3 .github/workflows/scripts/clippy-panic-gate.py
PASS — complete scope and production panic checks
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --lib
PASS — Generated facade documentation and 35 other files
cargo build --locked -p sley-cli --bins --release --features git-compat-i18n
PASS — Finished release profile, 49.02s
bash scripts/check-publish-pipeline.sh
PASS — exactly 31 publishable crates, topologically ordered
```

The publish-list validator also passed its negative self-tests (missing, bogus,
misordered, and duplicate entries are rejected). Direct nightly rustfmt with
`--edition 2024 --config skip_children=true --check` passed for all 294 changed
Rust files, including renamed files, and `git diff --check` passed. No workspace version was bumped.
The regular Clippy result reuses the successful 12.95-second check of the same
source; the all-feature and minimal-feature runs independently checked their
configurations. The final full build/test run followed the lint fixes. GitHub caught two
whitespace-only issues in renamed discovery files omitted by the initial scoped
formatter list; both were corrected and the complete added/modified/renamed
file set passes. The follow-up changes no executable behavior.

## Isolation checks that demonstrably fail

`cargo test -p sley --features full --test repository_policy_isolation
--test library_diagnostics` passed, as did these tests in the full workspace run.
The repository test creates both physical namespaces in each repository, with
different expected refs. Its two workers repeatedly observe actual ref
advertisements, worktree status paths, Unicode configuration snapshots, protocol
allow/deny decisions, and `allow=user` behavior.

The CWD test protects a different directory in each repository and requires both
to survive file removal. It additionally checks that unrelated empty directories
are pruned. This strengthens the interrupted-session test, whose second worker
protected its root and therefore could pass with the first worker's shared CWD.

Each temporary mutation below was made independently, tested, and restored in a
`finally` block. All five runs failed **test assertions**, not compilation. The
restored run then passed all four integration tests.

| Temporary regression | Observed failing check |
| --- | --- |
| `Namespace::prefix` uses one `OnceLock<String>` instead of its receiver | Wrong valid namespace/ref returned to one repository worker |
| `PrecomposeUnicode::is_enabled` uses one `OnceLock<bool>` | One repository's actual status filename uses the other repository's Unicode policy |
| `is_transport_allowed` uses one `OnceLock<TransportPolicy>` | The repository's explicit allow/deny or user-origin decision is wrong |
| `prune_empty_dirs` uses one `OnceLock<Option<PathBuf>>` for original CWD | One caller's protected directory is removed |
| Diagnostic emission uses the silent default instead of the current caller sink | The exact expected config-fatal bytes are absent from the caller's recording |

`library_diagnostics.rs` also checks concrete callback downcasting/source chains
and preserved I/O kinds. Core tests check nested and concurrent sinks, explicit
worker inheritance, byte/channel preservation, restoration after panic, and
silence outside a scope. CLI tests pin command status and typed rejection mapping.

## Pinned oracle and parity floors

The unchanged curated manifest validates **891 included scripts**. Environment
preflight passes. The runner used eight waves, Linux/SHA-1, and a 900-second
per-script timeout. Release-binary SHA-256:
`72d2327ce1290324bf3c48822aca8bb410817922c2b2c5354f72404fe092d3f7`.

The fresh oracle run selected the unchanged 13 PR scripts plus `t5509`, `t0050`,
`t1408`, `t3910`, and `t5613`. It completed with 16 passing scripts, two legitimate
skips, zero failures, zero aborts, and zero timeouts. Candidate: 13 passing
scripts, two skips, three partial failures, zero aborts, and zero timeouts.
As in CI, the candidate runner's raw exit 1 is not the regression-floor gate.

```text
bash .github/workflows/scripts/check-parity-floors.sh \
  /home/scratch/sley-cuts-parity/pr-summary.csv \
  .github/workflows/parity-pr-scripts.txt
PARITY FLOOR GATE: PASSED

bash .github/workflows/scripts/check-parity-floors.sh \
  /home/scratch/sley-cuts-parity/extended-without-238.csv \
  .github/workflows/parity-pr-scripts.txt
PARITY FLOOR GATE: PASSED
```

The PR summary contains exactly the required 13 scripts. The extended summary
excludes **only** `t5613-info-alternate.sh`, as authorized for the pre-existing
[sley#238](https://github.com/HeddleCo/sley/issues/238) discrepancy: `ok=9`, floor
10, failing cells 2/3/7/12. The unfiltered 18-script summary and failing floor log
are preserved; that is its only below-floor result. No gate, floor, waiver,
prerequisite, script selection, or comparison implementation was changed.

| Script | Candidate ok / floor | Oracle comparison |
| --- | --- | --- |
| `t0001-init.sh` | 103 / 102 | Exact TAP vector |
| `t0002-gitfile.sh` | 14 / 14 | Exact |
| `t0034-root-safe-directory.sh` | 0 / 0 | Matching environment skip |
| `t1006-cat-file.sh` | 290 / 290 | Existing partial coverage; not exact |
| `t1007-hash-object.sh` | 40 / 40 | Exact |
| `t1092-sparse-checkout-compatibility.sh` | 110 / 110 | Exact cells; raw script classifications differ |
| `t1300-config.sh` | 516 / 516 | Exact |
| `t1400-update-ref.sh` | 315 / 315 | Exact |
| `t1401-symbolic-ref.sh` | 25 / 25 | Exact |
| `t1500-rev-parse.sh` | 82 / 82 | Existing differing skip; not exact |
| `t1501-work-tree.sh` | 39 / 39 | Exact |
| `t3437-rebase-fixup-options.sh` | 10 / 10 | Existing partial coverage; not exact |
| `t5003-archive-zip.sh` | 78 / 78 | Exact |
| `t5509-fetch-push-namespaces.sh` | **15 / 4** | **All 15 cases pass; exact** |
| `t0050-filesystem.sh` | **13 / 6** | **Exact, including matching filesystem prerequisites** |
| `t1408-packed-refs.sh` | **3 / 3** | **Exact, including Unicode ref names** |
| `t3910-mac-os-precompose.sh` | 0 / 0 | Matching Linux skip; no macOS-native claim |
| `t5613-info-alternate.sh` | 9 / 10 | Authorized pre-existing #238 exclusion only |

A separate release-binary probe compares exit status, stdout bytes, and stderr
bytes against Git for seven local namespace cases (root, nested option, nested
environment, option overriding environment, missing namespace, logical hide-ref,
physical hide-ref) and four portable Unicode cases (short status, NFC and NFD
hash-object operands, and NUL-terminated index names). **All 11 match exactly.**
The existing CLI `alias_repository_switch_reapplies_precompose_to_expanded_tail`
test also passes in the full suite.

Exploratory probes outside that passing matrix found two older compatibility
limits: client-side `-c transfer.hideRefs` leaks into Sley's local peer config,
and `-c core.quotepath=false status --short` still quotes Unicode paths. Both
produce the same divergence with the preserved pre-cut audit binary (checksum
`27147700:37424200`, matching its original run metadata). The hide-ref proof above
configures the server repository, as Git requires. These observations do not
waive any floor or alter the required selection; this cut does not claim complete
Git parity or implement those separate compatibility features.

A fresh run of the preserved pre-cut audit executable against this same 18-script
selection yields **all 1,673 TAP cells identical** to the final candidate, comparing
script/cell ids, status, raw result, directive, and description. This establishes
that the remaining partial coverage and #238 failure are unchanged, rather than
relying only on aggregate floors. The archived executable checksum matches its
original audit metadata; the new runner metadata identifies the current invocation
worktree separately from that archived binary.

## Removal review and limits

The migration guide contains the per-deletion, whole-repository no-live-caller
inventory. Its remaining old-name hits are historical records or the explicitly
identified uninvoked extraction script. Live API links and the parity checklist
were reconciled. Mandatory normal Sley dependency nodes fall from **26 to 16**,
including the facade. All 31 publishable crates remain.

The `ObjectReader` push seam, single-use HTTP observation/CAS preconditions, Git
formats/transports, streaming/cancellation/budgets, and working caches remain.
Consumer repins/builds and the coordinated 0.10 version bump are release/adoption
steps. Per-task async diagnostic routing is deferred: these engines are
synchronous, so async hosts install a sink inside their blocking operation.
The full 891-script sweep and non-Linux/hash/platform lanes were not run locally.

Local evidence: `/home/scratch/sley-cuts-*.log` and
`/home/scratch/sley-cuts-parity/` (raw summaries, exact cells, comparison CSVs,
metadata, and both filtered/unfiltered floor results). Negative controls and the
portable oracle probe are retained as `sley-cuts-negative-controls.py` and
`sley-cuts-oracle-policy.py` in `/home/scratch`.
