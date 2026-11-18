# SLEY-267 round 4: security review fixes

## Summary

- Preserve unlimited trusted legacy install/parse defaults while exposing explicit `ResolveLimits` through `PackIndexOptions::with_resolve_limits` and `PackFile::parse_with_options`. The original index bytes remain identical at threads 1, 2 and 16.
- Add shared in-flight workspace reservations and cap actual worker creation to the workspaces that fit. Add a shared per-operation budget covering discovery, resolution and every replay, with a distinct `ResolveError::ResolveOps` variant and public accounting counters.
- Replace the deprecated atomic call; cap initial metadata reservation; document plain SHA-1 and provisional callback effects on the public fields/API. Exercise the portable visitor test target in Windows CI. No versions, release configuration or publishing changes.

## Closes

Closes #267
Closes #259

Source links below are pinned to `942e85bb`. Base `origin/main` at `61848386` was merged by plain merge `dcc20b93`; its #270 fixes and four lockfile bumps are inherited unchanged. This round follows C-1709 in the owner brief: trusted callers keep unlimited defaults; heddle explicitly sets its import policy. The historical [round-2 evidence](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/docs/SLEY_267_ROUND2_AUDIT.md) remains available; this report supersedes its transient-memory and legacy-default descriptions.

## Coverage table

| Original design-pass item | Current fix / source | Public API evidence |
|---|---|---|
| Unbounded base retention | [resolved.rs:600 (shared retained-byte gate)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L600); [resolved.rs:771 (last-dependent release)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L771); [resolved.rs:613 (nearest-retained-ancestor replay)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L613). The new transient gate is separate and shared too; see P2-3 below. | `live_base_bound_is_shared_and_replay_stays_within_peak_memory`; `last_dependent_releases_base_before_visiting_child`; R2 shared-live-gate / last-dependent-release RED output below. |
| Superlinear zlib probing | [resolved.rs:333 (sequential measured discovery)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L333); no every-offset scanner in this engine. Compressed replay work is capped as well. | `embedded_zlib_members_have_linear_discovery_cpu_cost`: actual original scanner restored in the R2 RED, rather than a synthetic spin. |
| Reservation before declared/remaining-budget checks | [resolved.rs:574 (every inflate)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L574); [resolved.rs:587 (every apply)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L587). Both spend object, remaining-byte and operation budgets before reservation; complete workspace admission precedes either call. | `declared_size_bomb_is_refused_before_allocation`; `delta_result_bomb_and_running_budgets_are_checked_before_visits`; instruction/result/replay and malformed-varint tests; 2^40 peak RSS/VmPeak evidence below. |
| Depth ≤50 before apply | Pre-existing on [origin/main's depth guard](https://github.com/HeddleCo/sley/blob/a71654f50de24b3c750f0df5f1829db7342491f6/crates/sley-pack/src/parallel_index.rs#L762), preserved at [resolved.rs:751](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L751) before replay/inflate/apply. | `depth_51_is_refused_before_visiting_the_deep_object`; R2 depth-before-apply RED. |
| SHA-1 collision admission | [Issue #267 Design, “What the heddle side does in the callback”](https://github.com/HeddleCo/sley/issues/267) assigns sha1dc over canonical header + body, oid comparison and collision rejection to heddle. [resolved.rs:104 (oid field contract)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L104); [index.rs:7 (idx contract)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/index.rs#L7). Sley uses ordinary SHA-1/SHA-256. | `caller_admission_rejection_is_preserved_without_returning_a_build` tests propagation of caller rejection, not the external sha1dc algorithm. R4 caller-verifier-error RED and public field-doc RED/GREEN below. |
| Thin/truncated/cancelled packs return no partial index | [resolved.rs:981 (completion refusal)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L981); [resolved.rs:993 (missing-output refusal)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L993); discovery finishes before any visit. [resolved.rs:174 (caller staging contract)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L174). Thin suffixes can follow provisional visits; no index/build is returned. | `thin_pack_is_an_unresolved_base_error` stages and discards a valid prefix, with 1 callback and 0 admitted objects; truncation/cancellation tests remain. R4 thin-completion RED and GREEN below. |

## Security review coverage

| Finding | Fix / exact source | RED / GREEN evidence |
|---|---|---|
| P1-1: stable-1.100 CI compile failure | [resolved.rs:600 (try_update retained gate)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L600); the operation/transient gates use `try_update` too. | `deprecated-atomic` reverts the original call and compiles the public API test target with `RUSTFLAGS=-D warnings`: compilation RED, as expected for this finding. The seven requested local commands and every GitHub step are checked separately below. |
| P1-2: trusted install/parse regression | [resolved.rs:52 (unlimited trusted policy)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L52); [parallel_index.rs:17 (default policy)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/parallel_index.rs#L17); [parallel_index.rs:30 (explicit opt-in limits)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/parallel_index.rs#L30); [parallel_index.rs:36 (depth compatibility)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/parallel_index.rs#L36); [read.rs:22 (parse options)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/read.rs#L22). `PackReadLimits` keeps its existing fields/defaults; install/fetch/clone call sites need no change. | `trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply`: an explicit 128-byte cap refuses a 192-byte decoded fixture; both trusted legacy routes accept it and preserve idx equality at 1/2/16 threads. Three RED probes revert the default and each legacy route; the route probes scale the previous 16-GiB policy to 128 bytes for an inexpensive executed regression. |
| P2-3: transient working memory multiplies with threads | New `max_in_flight_bytes` at [resolved.rs:33](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L33), default 1 GiB; [resolved.rs:536 (atomic admission / wait)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L536); [resolved.rs:703 (lease surrounds entire walk and replay)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L703); [resolved.rs:875 (complete workspace calculation)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L875); [resolved.rs:792 (limit actual threads, including idle arenas)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L792). Public counters are [resolved.rs:67 (ResolveMetrics)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L67). | `in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory`: scaled 16-root compressible-blob/copy-delta pressure, 8 MiB bodies, 24 MiB flight gate, 8 MiB retained gate. Portable accounting and Linux process RSS assertions. Separate RED probes remove both admission layers, only worker sizing, lease lifetime, error cleanup, base or instruction workspace components. |
| P2-4: tiny replay operations evade byte budgets | New `max_resolve_ops` at [resolved.rs:35](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L35), default 100M; shared atomic check [resolved.rs:83](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L83); distinct `ResolveError::ResolveOps { limit, attempted }`. Charge [resolved.rs:388 (discovery)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L388), [resolved.rs:574 (inflate)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L574), [resolved.rs:587 (apply)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L587). Replay has no alternate decoder. Heddle maps this variant to `IMPORT_BUDGET_EXCEEDED{resolve_ops}` and supplies max(1M, 8 × object count). | `resolve_ops_counts_discovery_inflate_apply_and_replay` verifies exact counts with/without retention, each refusal at the next op, discovery and a 16-root shared limit. `million_tiny_leaves_stop_at_resolve_op_budget`: 16,867,462-byte pack, depth 50, 1M leaves; refuses at charged count 1,010,051 / attempted 1,010,052, after 125 visits. With the ceiling removed, >10M ops execute before watchdog cancellation. |
| P3-5: reserve unparsed declared count | [resolved.rs:362 (pack_entry_prealloc)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L362); [resolved.rs:401 (fallible growth only after entries parse)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L401). Count/input validation and configured max_objects checks precede reservation. | `declared_count_preallocation_is_capped_before_parsing`: 1M declared entries, 3 MB of invalid entry bytes; initial reservation must stay below an 8 MiB VmPeak increase. Reverting the preallocation cap goes RED. |
| P3-6: unmissable plain-SHA-1 contract | [resolved.rs:105 (ResolvedObject::oid field docs)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L105); [index.rs:7 (PackIndexBuild and index field docs)](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/index.rs#L7). sha1dc is caller-owned; plain SHA-1 checksums are not collision-detection evidence. | Rendered public field sections are checked by committed `tests/public_docs.py` for the SHA-1 and sha1dc contract (also enforced after the CI documentation build). Removing each field's doc guard gives public-doc RED; restoration gives GREEN. Caller-verifier-error is an executed public API behavior RED. These checks do not claim to implement/test sha1dc itself. |
| P3-7: provisional callbacks on late thin packs | Choose the explicit staging contract: [resolved.rs:174](https://github.com/HeddleCo/sley/blob/942e85bbafedb5bb73908b0705e96bf2dd1a46a4/crates/sley-pack/src/resolved.rs#L174). REF identities require resolution/hashing, so root visits can precede discovery of an unresolved suffix. Callers must commit staged effects only after Ok. | `thin_pack_is_an_unresolved_base_error` explicitly clears its provisional store and admits 0 objects. Removing both completion guards returns a partial index and goes RED. |
| P3-8: Linux memory helper compilation | The reviewed `5913558e` already had the entire test under `#[cfg(target_os = "linux")]` at [tests/resolved.rs:635](https://github.com/HeddleCo/sley/blob/5913558e430caa5a85f8e508586486155f299077/crates/sley-pack/tests/resolved.rs#L635). Current guard is tests/resolved.rs:658. No unguarded call exists in that test. New portable tests guard only their /proc subprocess/RSS sections. | The Windows job now runs committed `tests/windows_cfg_probe.py`: remove the Linux guard, compile the public test target and require missing memory_child/peak_bytes errors, then restore the original bytes before `cargo test --locked -p sley-pack --test resolved`. The hosted RED and restored GREEN output is appended to the PR body when that pushed run completes. The existing cfg fence is retained; this probe executes on Windows. |

## Self-attack

Let P = borrowed pack length, N = checked object count (≤ max_objects), T = max_total_decoded, O = max_object_bytes, B = max_live_base_bytes, F = max_in_flight_bytes, and R = max_resolve_ops. For each descriptor let result length be r and delta-stream length be s; C is the maximum result length. One complete workspace reserves `max(C, max_delta(C + r + s))` bytes, with overflow checks. It covers the old replay/base body, current instructions and new result simultaneously. A workspace above F is refused before any object buffer is allocated. Actual worker count is bounded by both the requested CPU count and the complete workspaces that fit F.

The decoded-buffer bound is **F + B across all threads**. RSS also includes P, O(N) metadata/index, the fixed 64 KiB discovery scratch buffer, per-admitted-worker stacks/zlib state, allocator slack, and callback-owned storage. The RSS regression uses an isolated process and a stated 64 MiB slack for its input/builder, allocator, thread stacks/zlib and 32-entry metadata. An arbitrary caller-owned visitor can retain memory or run arbitrary CPU; callers must stage/bound that output. `PackFile` deliberately materializes output and trusted defaults are deliberately unlimited. The bounded visitor never silently chooses trusted limits.

| Route attempted | Boundary, test, or why no bypass exists |
|---|---|
| 16 roots multiply transient bases/results/instructions | One atomic shared F counter; admission occurs before allocating any walk buffer. Full-workspace leases and bounded actual worker creation both matter: removing both gives ~256 MiB reservations / ~272 MiB RSS under a 24 MiB gate. Keeping admission but spawning all idle workers still leaves allocator arenas outside the intended RSS slack; its own guard-revert is RED. Portable counter and Linux RSS test cover both. |
| An old replay body, instruction stream or new result is uncharged | The workspace formula covers two bodies plus one delta, rather than only the retained cache. The same lexical walk lease encloses every replay iteration. `flight_reservations_release_after_errors_and_waiters_observe_cancel` rejects a gate that fits two bodies but cannot also fit instructions; removing base or instruction terms goes RED. R2 zero-cache/replay scenarios remain. |
| Release charges early while buffers are borrowed, or leak them on an error | The Flight lease is declared before all walk buffers and drops last. Public visitor assertions require its reservation while borrowing body/instructions; an immediate-drop mutation is RED. Error/cancel cases require the counter to return to zero; disabling Drop's release is RED. Retained Drop frees its Vec before releasing its B charge. |
| Workers deadlock while each holds a base and waits for result space | Reserve the entire workspace before entering walk; no subsequent partial acquisition while holding a transient base. Actual threads fit that workspace count. A workspace that cannot fit returns an error, rather than waiting forever. Condvar waits poll cancellation; no state/visitor lock is held while waiting. |
| Replays cost zlib initialization/mutex work but almost no bytes | Every discovery-member inflation, every resolution/replay inflation and every delta apply spends one shared R operation before doing it. Exact 8-versus-12-operation scenarios and independent discovery/inflate/apply guard-reverts cover all three sites. The million-leaf vector stops at the exact shared ceiling; removing it runs >10× that budget before the independent watchdog cancels. |
| Per-thread operation budgets or counts released/reused | One private atomic op counter per resolution, no per-thread copies and no decrements. Sixteen roots share cap 120 and refuse attempted op 121. Metrics fields are private; callers cannot reset/bypass counters during a call. Use fresh metrics for each resolution. |
| Large-byte or empty-DEFLATE-block replay amplification | Historical inflated-stream and decoded-result charges each cap at T across threads, including replay; compressed work caps at P + T. R2 replayed-inflate, shared-result and compressed-replay RED tests remain. R adds a separate constant-operation bound. |
| Nested deltas / adversarial forward REF order | Default depth 50 checked before apply; no recursive descent. DFS frames are bounded by depth and N. `depth_51_is_refused_before_visiting_the_deep_object`; forward-REF/duplicate-OID fixtures. Explicit depth overrides stay finite/u16 representable. |
| Missing REF, rootless REF cycle, or many distinct missing IDs | New visitor has no external callback. Completion refuses. Ordered map plus one pending snapshot costs O(N log N), with no repeated scan past missing IDs. Thin, cyclic/missing-offset and 32,768-missing-ID CPU tests. Legacy external-base callbacks are caller-owned trusted inputs, not a hidden route inside resolve_pack. |
| OFS cycle or offset into a header | OFS distance must be positive and points strictly backwards; binary search requires an actual earlier descriptor. Layered parser/scheduler/completion guards. Existing cyclic/missing-offset test; the R2 layered-guard RED is diagnostic-specific and does not claim a partial index escaped remaining guards. |
| Huge instructions count, zero-length copy, malformed ranges/varints | Declared stream and result sizes checked independently before allocation, then remaining-byte/operation/workspace checks. Varints reject shifted-out bits. Copy size zero means 65,536 bytes, not no progress; every opcode advances its input cursor, output bounds precede extension. Bomb, overflow, malformed-command and zero-copy tests, including peak-memory RED. |
| Many small/empty objects evade byte caps / header count reserves huge metadata | max_objects and physical input length checked first. Initial reserve ≤65,536 descriptors; fallible geometric growth is tied to parsed entries and checked count. Empty-object and unparsed-count memory tests. Empty objects still spend discovery/resolution ops; zero workspace does not waive N/R limits. |
| Duplicate OIDs / racing child claims / unverified returned instructions | Roots claimed by atomic cursor; OFS child lists taken once and REF lists removed once under lock. Every descriptor has one base kind. Real SHA-1/SHA-256 fixtures at threads 1/2/16 check original idx equality, one visit per physical offset, canonical ordinary digest and independent application of instructions. |
| Thin/truncated/cancelled pack yields partial external effects | No PackIndexBuild on error. Full discovery precedes visits for truncation. Late thin REF failures can follow visits, hence explicit staged admission; its test records 1 provisional callback, 0 admitted objects. Cancellation is checked in discovery, walk/admission waits, coordinator and before returning output. Sort work is bounded by N; effects remain provisional until Ok. |
| SHA-1 collision / wrong admission hash / callback rejection | Ordinary SHA-1 is explicitly NOT sha1dc on both oid and idx fields. Heddle must re-hash canonical header + body, compare oid, reject collision/mismatch and stage admission. Typed rejection is preserved and cancels other workers. Public documentation and verifier-propagation guard-reverts cover this boundary; no claim of testing heddle's sha1dc algorithm. |
| Wrappers / second engine caller / unsafe or mutable input bypass | Every whole-pack index/parse wrapper calls the same private resolver. PackIndexOptions' new field is private, preserving existing public construction; PackReadLimits' existing fields remain unchanged. Explicit opt-in caps cover both wrappers; trusted defaults cannot be selected by pack bytes. Safe borrowed input cannot be concurrently mutated, and callback borrows cannot escape without copying. Other targeted decoder/scan APIs keep their own existing contracts. |

## Test evidence

All behavior probes compile and execute tests through public APIs. Seventeen probes produced exit 101. The deprecated-atomic probe is intentionally a **compile RED** of the public API test target because compilation failure is the finding. The other sixteen are executed failing tests, not compile failures. Two additional probes inspect rendered **public field documentation** (exit 1 when each warning is removed). Windows CI additionally reverts/restores the pre-existing Linux cfg fence and requires a compile RED of the public test target before its positive run. Every mutation is restored. Compiler progress, duplicate child-harness lines and large returned-index debug arrays are omitted where indicated; excerpts below are actual output.

The shared-flight probe removes both admission layers; the worker-arena probe removes only worker sizing. The legacy route probes restore the former default-policy routing with its decoded cap scaled to 128 bytes, alongside the separate exact default-policy revert. The no-op-budget vector has 1M leaves at depth 50 (roughly 50M replay pairs / 100M inflate+apply ops); a 30-second independent watchdog bounds a guard-reverted test run.

### R4 RED: deprecated-atomic

```text
error: use of deprecated method `std::sync::atomic::Atomic::<usize>::fetch_update`: renamed to `try_update` for consistency
   --> crates/sley-pack/src/resolved.rs:602:14
    |
602 |             .fetch_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |used| {
    |              ^^^^^^^^^^^^
    |
    = note: `-D deprecated` implied by `-D warnings`
    = help: to override `-D warnings` add `#[allow(deprecated)]`
help: replace the use of the deprecated method
    |
602 -             .fetch_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |used| {
602 +             .try_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |used| {
    |

error: could not compile `sley-pack` (lib) due to 1 previous error
```

### R4 RED: trusted-default-policy

```text
running 1 test

thread 'trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply' (997) panicked at crates/sley-pack/tests/resolved.rs:1229:5:
assertion `left == right` failed
  left: 17179869184
 right: 18446744073709551615
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply ... FAILED

failures:

    trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.00s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: legacy-index-route

```text
running 1 test

thread 'trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply' (1328) panicked at crates/sley-pack/tests/resolved.rs:1256:34:
trusted default index: InvalidFormat("ResolveLimits::max_total_decoded exceeded")
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply ... FAILED

failures:

    trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.00s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: legacy-parse-route

```text
running 1 test

thread 'trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply' (1659) panicked at crates/sley-pack/tests/resolved.rs:1261:58:
trusted parse: InvalidFormat("ResolveLimits::max_total_decoded exceeded")
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply ... FAILED

failures:

    trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.00s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: shared-flight-gates

```text
running 1 test

flight: peak=268435680, gate=25165824, after=0
flight RSS: peak=285163520, gate+live+64MiB=100663296
test in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory ... FAILED

failures:

    in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 7.62s



thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (1992) panicked at crates/sley-pack/tests/resolved.rs:372:5:

thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (1994) panicked at crates/sley-pack/tests/resolved.rs:1323:9:
in-flight working memory exceeded RSS bound
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace




test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 7.65s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: worker-arena-bound

```text
running 1 test

flight: peak=16777230, gate=25165824, after=0
flight RSS: peak=285347840, gate+live+64MiB=100663296
test in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory ... FAILED

failures:

    in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 16.37s



thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (2341) panicked at crates/sley-pack/tests/resolved.rs:372:5:

thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (2343) panicked at crates/sley-pack/tests/resolved.rs:1323:9:
in-flight working memory exceeded RSS bound
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace




test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 16.39s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: flight-lease-lifetime

```text
running 1 test

test in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory ... FAILED

failures:

    in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 6.33s



thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (2690) panicked at crates/sley-pack/tests/resolved.rs:372:5:

thread '<unnamed>' (2693) panicked at crates/sley-pack/tests/resolved.rs:1298:13:
lease released while callback borrows buffers
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (2692) panicked at crates/sley-pack/tests/resolved.rs:1310:6:
workers serialize within one shared gate: Pack(InvalidFormat("parallel pack worker panicked"))




test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 6.34s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: flight-error-release

```text
running 1 test

thread 'flight_reservations_release_after_errors_and_waiters_observe_cancel' (3024) panicked at crates/sley-pack/tests/resolved.rs:1372:9:
assertion `left == right` failed: all workspace leases must drop on error
  left: 65548
 right: 0
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test flight_reservations_release_after_errors_and_waiters_observe_cancel ... FAILED

failures:

    flight_reservations_release_after_errors_and_waiters_observe_cancel

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.02s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: delta-stream-workspace

```text
running 1 test

thread 'flight_reservations_release_after_errors_and_waiters_observe_cancel' (3356) panicked at crates/sley-pack/tests/resolved.rs:1409:6:
two bodies alone leave no room for the delta stream: [large returned index debug value omitted]
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test flight_reservations_release_after_errors_and_waiters_observe_cancel ... FAILED

failures:

    flight_reservations_release_after_errors_and_waiters_observe_cancel

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.08s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: replay-base-workspace

```text
running 1 test

thread 'flight_reservations_release_after_errors_and_waiters_observe_cancel' (3691) panicked at crates/sley-pack/tests/resolved.rs:1370:9:
assertion `left == right` failed
  left: 2
 right: 1
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test flight_reservations_release_after_errors_and_waiters_observe_cancel ... FAILED

failures:

    flight_reservations_release_after_errors_and_waiters_observe_cancel

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.02s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: resolve-op-ceiling

```text
running 1 test
tiny replay: pack=16867462 bytes, objects=1000051, ops=10893649, limit=1010051, visits=98961

thread 'million_tiny_leaves_stop_at_resolve_op_budget' (4024) panicked at crates/sley-pack/tests/resolved.rs:1580:5:
operation budget must fire during replay: Err(Pack(Cancelled))
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test million_tiny_leaves_stop_at_resolve_op_budget ... FAILED

failures:

    million_tiny_leaves_stop_at_resolve_op_budget

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 30.64s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: discovery-op-charge

```text
running 1 test

thread 'resolve_ops_counts_discovery_inflate_apply_and_replay' (4357) panicked at crates/sley-pack/tests/resolved.rs:1437:9:
assertion `left == right` failed
  left: 5
 right: 8
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test resolve_ops_counts_discovery_inflate_apply_and_replay ... FAILED

failures:

    resolve_ops_counts_discovery_inflate_apply_and_replay

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.00s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: inflate-op-charge

```text
running 1 test

thread 'resolve_ops_counts_discovery_inflate_apply_and_replay' (4689) panicked at crates/sley-pack/tests/resolved.rs:1437:9:
assertion `left == right` failed
  left: 5
 right: 8
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test resolve_ops_counts_discovery_inflate_apply_and_replay ... FAILED

failures:

    resolve_ops_counts_discovery_inflate_apply_and_replay

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.00s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: apply-op-charge

```text
running 1 test

thread 'resolve_ops_counts_discovery_inflate_apply_and_replay' (5021) panicked at crates/sley-pack/tests/resolved.rs:1437:9:
assertion `left == right` failed
  left: 6
 right: 8
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test resolve_ops_counts_discovery_inflate_apply_and_replay ... FAILED

failures:

    resolve_ops_counts_discovery_inflate_apply_and_replay

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.00s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: bounded-preallocation

```text
running 1 test

declared count: virtual peak growth=26906624, bound=8388608
test declared_count_preallocation_is_capped_before_parsing ... FAILED

failures:

    declared_count_preallocation_is_capped_before_parsing

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.29s



thread 'declared_count_preallocation_is_capped_before_parsing' (5353) panicked at crates/sley-pack/tests/resolved.rs:372:5:

thread 'declared_count_preallocation_is_capped_before_parsing' (5355) panicked at crates/sley-pack/tests/resolved.rs:1611:9:
reservation followed unparsed header count
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace




test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.30s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: thin-completion

```text
running 1 test

thread 'thin_pack_is_an_unresolved_base_error' (5686) panicked at crates/sley-pack/tests/resolved.rs:122:10:
vector must be refused: [large returned index debug value omitted]
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test thin_pack_is_an_unresolved_base_error ... FAILED

failures:

    thin_pack_is_an_unresolved_base_error

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.00s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 RED: caller-verifier-error

```text
running 1 test

thread 'caller_admission_rejection_is_preserved_without_returning_a_build' (6017) panicked at crates/sley-pack/tests/resolved.rs:986:5:
assertion failed: matches!(result, Err(ResolveError::Visitor(AdmissionRejected)))
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test caller_admission_rejection_is_preserved_without_returning_a_build ... FAILED

failures:

    caller_admission_rejection_is_preserved_without_returning_a_build

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 25 filtered out; finished in 0.02s

error: test failed, to rerun pass `-p sley-pack --test resolved`
```

### R4 public-doc RED: oid-docs

```text
Traceback (most recent call last):
  File "/tmp/sley267-r4-public-docs-check.py", line 7, in <module>
    assert 'sha1dc' in s[start:end], f'{page}::{field}: missing collision-detection warning'
           ^^^^^^^^^^^^^^^^^^^^^^^^
AssertionError: resolved/struct.ResolvedObject.html::oid: missing collision-detection warning
```

### R4 public-doc RED: idx-docs

```text
public docs: resolved/struct.ResolvedObject.html::oid identifies plain SHA-1 and caller sha1dc requirement
Traceback (most recent call last):
  File "/tmp/sley267-r4-public-docs-check.py", line 7, in <module>
    assert 'sha1dc' in s[start:end], f'{page}::{field}: missing collision-detection warning'
           ^^^^^^^^^^^^^^^^^^^^^^^^
AssertionError: struct.PackIndexBuild.html::index: missing collision-detection warning
```

### Restored GREEN

```text
delta-result bomb: peak=5955584, growth=1814528
delta-result bomb: peak virtual growth=0
bomb: peak=5861376 bytes, growth=1142784 bytes
late thin pack: 1 provisional callback, 0 admitted objects, no returned index
resolve ops: live=1024, exact successful count=8; budget=7 refused attempted=8
trusted defaults: unlimited; explicit 128-byte cap refused; 192 decoded bytes accepted by both legacy paths
resolve ops: live=0, exact successful count=12; budget=11 refused attempted=12
resolve ops: 16 roots share cap=120, attempted=121
flight: visitor/cancel errors release all leases; oversized workspace refused before allocation
declared count: virtual peak growth=0, bound=8388608
tiny replay: pack=16867462 bytes, objects=1000051, ops=1010051, limit=1010051, visits=125
flight: peak=16777230, gate=25165824, after=0
flight RSS: peak=40894464, gate+live+64MiB=100663296
test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.39s
```

Both compression backends:

```text
default: test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.39s
pure-rust: test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 23.82s
```

Rendered public field documentation, guards restored:

```text
public docs: resolved/struct.ResolvedObject.html::oid identifies plain SHA-1 and caller sha1dc requirement
public docs: struct.PackIndexBuild.html::index identifies plain SHA-1 and caller sha1dc requirement
```

## Verification

All seven requested commands passed on stable rustc **1.100.0** / clippy **0.1.100**. Workspace: **3,812 passed, 0 failed, 2 ignored** across 224 Cargo test summaries. Documentation warnings were denied. The CI test identities were used; the isolated target directory is outside the source tree.

| Command | Exit | Time |
|---|---:|---:|
| `cargo build --locked --workspace --tests` | 0 | 68.55 s |
| `cargo fmt --all --check` | 0 | 3.56 s |
| `python3 .github/workflows/scripts/clippy-panic-gate.py` | 0 | 10.48 s |
| `cargo clippy --locked --workspace --all-targets --all-features --no-deps -- -D warnings` | 0 | 29.79 s |
| `cargo test --locked --workspace` | 0 | 145.04 s |
| `cargo doc --locked --workspace --no-deps --lib` | 0 | 14.29 s |
| `bash scripts/check-publish-pipeline.sh` | 0 | 0.08 s |

GitHub check and step conclusions are appended to the PR body after the pushed head finishes; this committed file records completed local/guard evidence rather than predicting hosted results. The PR is marked Ready only after every hosted check passes.

The parent contract's extra export-lint command does not exist in Sley; it is outside the seven requested checks:

```text
error: no such command: `xtask`
```

## Historical R2 guard output

The following original-design RED excerpts are preserved verbatim from the already-delivered round-2 audit; they were run in that round, not rerun as round-4 mutations. Current positive coverage is rerun below/above on the refreshed base.

### RED: shared-live-gate — live-base ceiling

```text
live bases: baseline=6225920, peak=358993920, cap=8388608 bytes (16 workers)
test live_base_bound_is_shared_and_replay_stays_within_peak_memory ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 4.04s
thread 'live_base_bound_is_shared_and_replay_stays_within_peak_memory' (331) panicked at crates/sley-pack/tests/resolved.rs:360:5:
thread 'live_base_bound_is_shared_and_replay_stays_within_peak_memory' (333) panicked at crates/sley-pack/tests/resolved.rs:652:5:
retained bases exceeded shared gate: peak=358993920
```

### RED: last-dependent-release — release after the last dependent

```text
last-dependent release: peak=58634240 bytes
test last_dependent_releases_base_before_visiting_child ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 1.65s
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 1.66s
thread 'last_dependent_releases_base_before_visiting_child' (676) panicked at crates/sley-pack/tests/resolved.rs:360:5:
thread 'last_dependent_releases_base_before_visiting_child' (678) panicked at crates/sley-pack/tests/resolved.rs:692:5:
completed ancestors still retained: 58634240
```

### RED: original-every-offset-scan — original speculative scan restored

```text
crafted nested-member pack: CPU ticks=796, bound=400
test embedded_zlib_members_have_linear_discovery_cpu_cost ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 8.29s
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 8.30s
thread 'embedded_zlib_members_have_linear_discovery_cpu_cost' (1006) panicked at crates/sley-pack/tests/resolved.rs:360:5:
thread 'embedded_zlib_members_have_linear_discovery_cpu_cost' (1008) panicked at crates/sley-pack/tests/resolved.rs:1096:5:
CPU exceeded linear-input budget
```

### RED: declared-object-size — whole/instruction declaration size guard

```text
test declared_size_bomb_is_refused_before_allocation ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s
thread 'declared_size_bomb_is_refused_before_allocation' (1337) panicked at crates/sley-pack/tests/resolved.rs:360:5:
thread 'declared_size_bomb_is_refused_before_allocation' (1339) panicked at crates/sley-pack/tests/resolved.rs:379:5:
assertion failed: error(&p, ResolveLimits::default()).contains("max_object_bytes")
```

### RED: declared-delta-result — isolated delta-result reservation guard

```text
delta-result bomb: peak=5750784, growth=1601536
delta-result bomb: peak virtual growth=69222400
test delta_result_bomb_and_running_budgets_are_checked_before_visits ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s
thread 'delta_result_bomb_and_running_budgets_are_checked_before_visits' (331) panicked at crates/sley-pack/tests/resolved.rs:364:5:
thread 'delta_result_bomb_and_running_budgets_are_checked_before_visits' (333) panicked at crates/sley-pack/tests/resolved.rs:441:9:
declared result reserved virtual memory before refusal
```

### RED: declared-instructions — actual instruction-stream size guard

```text
copy overrun: peak=5873664, growth=1667072
test delta_commands_cannot_overrun_or_avoid_making_progress ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.02s
thread 'delta_commands_cannot_overrun_or_avoid_making_progress' (1995) panicked at crates/sley-pack/tests/resolved.rs:360:5:
thread 'delta_commands_cannot_overrun_or_avoid_making_progress' (1997) panicked at crates/sley-pack/tests/resolved.rs:891:5:
assertion failed: error(&vector(b"abcd", &huge),
```

### RED: depth-before-apply — depth guard

```text
test depth_51_is_refused_before_visiting_the_deep_object ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.01s
thread '<unnamed>' (2333) panicked at crates/sley-pack/tests/resolved.rs:472:13:
assertion failed: o.depth <= 50
thread 'depth_51_is_refused_before_visiting_the_deep_object' (2332) panicked at crates/sley-pack/tests/resolved.rs:478:5:
assertion failed: pack_message(e).contains("observed depth 51")
```

### RED: caller-admission-boundary — caller verifier propagation

```text
test caller_admission_rejection_is_preserved_without_returning_a_build ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.01s
thread 'caller_admission_rejection_is_preserved_without_returning_a_build' (2655) panicked at crates/sley-pack/tests/resolved.rs:943:5:
assertion failed: matches!(result, Err(ResolveError::Visitor(AdmissionRejected)))
```

### RED: thin-completion — both unresolved-output completeness checks

```text
test thin_pack_is_an_unresolved_base_error ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s
thread 'thin_pack_is_an_unresolved_base_error' (2983) panicked at crates/sley-pack/tests/resolved.rs:117:10:
vector must be refused: [returned PackIndexBuild debug bytes omitted]
```

### RED: truncated-discovery — discovery no-progress refusal

```text
test truncation_and_trailing_bytes_are_errors ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s
thread 'truncation_and_trailing_bytes_are_errors' (3310) panicked at crates/sley-pack/tests/resolved.rs:560:5:
assertion `left == right` failed: discovery must complete before visits
  left: 2
 right: 0
```

### RED: cancel-polls — worker and coordinator cancellation polls

```text
test cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.01s
thread 'cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error' (3638) panicked at crates/sley-pack/tests/resolved.rs:589:5:
assertion `left == right` failed
  left: 48
 right: 1
```

### RED: replayed-inflate-work — replayed work charges

```text
test replay_work_cannot_exceed_the_shared_decoded_budget ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.02s
thread 'replay_work_cannot_exceed_the_shared_decoded_budget' (3966) panicked at crates/sley-pack/tests/resolved.rs:731:6:
repeated root inflation must spend work budget: [returned PackIndexBuild debug bytes omitted]
```

### RED: shared-replayed-result-work — delta-result work charge

```text
test replayed_delta_result_budget_is_shared_across_workers ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.11s
thread 'replayed_delta_result_budget_is_shared_across_workers' (4294) panicked at crates/sley-pack/tests/resolved.rs:764:6:
replayed delta results must spend a shared budget: [returned PackIndexBuild debug bytes omitted]
```

### RED: compressed-replay-work — compressed replay charge

```text
test low_output_deflate_replay_spends_encoded_work_budget ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.03s
thread 'low_output_deflate_replay_spends_encoded_work_budget' (4637) panicked at crates/sley-pack/tests/resolved.rs:1169:6:
encoded replay work must also be bounded: [returned PackIndexBuild debug bytes omitted]
```

### RED: entry-size-overflow — entry high-bit overflow guard

```text
test overflowing_entry_and_delta_size_varints_are_refused ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s
thread 'overflowing_entry_and_delta_size_varints_are_refused' (4965) panicked at crates/sley-pack/tests/resolved.rs:117:10:
vector must be refused: [returned PackIndexBuild debug bytes omitted]
```

### RED: delta-size-overflow — delta high-bit overflow guard

```text
test overflowing_entry_and_delta_size_varints_are_refused ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s
thread 'overflowing_entry_and_delta_size_varints_are_refused' (5293) panicked at crates/sley-pack/tests/resolved.rs:117:10:
vector must be refused: [returned PackIndexBuild debug bytes omitted]
```

### RED: copy-output-pre-growth — output bound before Vec extension

```text
copy overrun: peak=274051072, growth=269975552
test delta_commands_cannot_overrun_or_avoid_making_progress ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.12s
thread 'delta_commands_cannot_overrun_or_avoid_making_progress' (5621) panicked at crates/sley-pack/tests/resolved.rs:360:5:
thread 'delta_commands_cannot_overrun_or_avoid_making_progress' (5623) panicked at crates/sley-pack/tests/resolved.rs:829:9:
copy output grew beyond its declared zero bytes
```

### RED: many-small-object-count — metadata count ceiling

```text
test many_empty_objects_are_bounded_before_metadata_allocation ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.08s
thread 'many_empty_objects_are_bounded_before_metadata_allocation' (5951) panicked at crates/sley-pack/tests/resolved.rs:360:5:
thread 'many_empty_objects_are_bounded_before_metadata_allocation' (5953) panicked at crates/sley-pack/tests/resolved.rs:985:6:
count bound before metadata: [returned PackIndexBuild debug bytes omitted]
```

### RED: ofs-cycle-layers — both OFS cycle checks; completion still refuses

```text
test cyclic_or_missing_offsets_and_ref_bases_never_produce_an_index ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s
thread 'cyclic_or_missing_offsets_and_ref_bases_never_produce_an_index' (6296) panicked at crates/sley-pack/tests/resolved.rs:910:9:
assertion failed: error(&finish(p, 1), ResolveLimits::default()).contains(diagnostic)
```

### RED: quadratic-missing-ref-search — repeated search past missing IDs restored

```text
distinct missing REF bases: CPU ticks=763, bound=400
test many_missing_ref_bases_have_linear_lookup_cpu_cost ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 8.06s
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 19 filtered out; finished in 8.07s
thread 'many_missing_ref_bases_have_linear_lookup_cpu_cost' (330) panicked at crates/sley-pack/tests/resolved.rs:364:5:
thread 'many_missing_ref_bases_have_linear_lookup_cpu_cost' (332) panicked at crates/sley-pack/tests/resolved.rs:1116:5:
CPU exceeded linear-input budget
```

## Production budget

Cumulative against origin/main at 61848386, excluding tests, CI test wiring, fixtures and evidence:

| File | Added | Deleted | Net |
|---|---:|---:|---:|
| `crates/sley-pack/src/index.rs` | 5 | 0 | +5 |
| `crates/sley-pack/src/lib.rs` | 2 | 0 | +2 |
| `crates/sley-pack/src/parallel_index.rs` | 78 | 756 | -678 |
| `crates/sley-pack/src/read.rs` | 17 | 0 | +17 |
| `crates/sley-pack/src/resolved.rs` | 1012 | 0 | +1012 |
| **Total** | **1114** | **756** | **+358** |

Gross production churn: **1,870 lines** (1,114 added, 756 deleted). Net **+358 / +420 target**, below the **+525 stop**. Round 2 was +118; round 4 adds +240 net. No budget band used.

Workspace version is still 0.13.0, matching the =0.13.0 embedding examples. Cargo.lock contains only main's inherited #270 changes, not lane edits. No version bump, release configuration edit, publish, rebase, force push or merge of the PR.
