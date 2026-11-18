# SLEY-267 round 2: hostile-input coverage audit

## Summary

- Add shared work budgets for **every** inflation and delta-result allocation, including replay. Bound compressed work too, so empty DEFLATE blocks cannot amplify repeated decoding. Exhausting replay budgets refuses the pack.
- Reject overflowing entry/delta size varints before allocation, use an ordered REF map for worst-case lookup bounds, handle worker-creation failures as errors, and check cancellation before returning the index.
- Add crafted-pack CPU bounds, resident/virtual peak-memory assertions, malformed-delta and callback-admission tests. Preserve the public APIs, exactly-once visits, raw verified instructions, and original idx bytes at threads 1, 2 and 16. Keep this PR **draft**.

## Closes

Closes #267
Closes #259

## Coverage table

Source links are pinned to `e8ca3f41`. `origin/main` comparisons use `a71654f5`; no other branch/worktree was changed. This is a **CHANGES** round: the audit found uncharged repeated/low-output replay work, overflowing shift values, and missing CPU/peak-memory coverage.

| Design-pass defect | Fix or pre-existing guard (file:line) | Public API RED test | Pasted probe output below |
|---|---|---|---|
| 1. Unbounded base retention | [resolved.rs:423 (shared atomic gate)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L423); [resolved.rs:593 (last-dependent release)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L593); [resolved.rs:436 (nearest-retained-ancestor replay)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L436); [resolved.rs:619 (worker path)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L619) | `live_base_bound_is_shared_and_replay_stays_within_peak_memory`; `last_dependent_releases_base_before_visiting_child` | shared-live-gate; last-dependent-release |
| 2. Every-offset zlib probing | [resolved.rs:229 (sequential discovery)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L229) walks exactly the header count; [resolved.rs:291 (next offset is the measured member end)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L291). Original scanner deleted. | `embedded_zlib_members_have_linear_discovery_cpu_cost` | original-every-offset-scan |
| 3. Reservation before remaining-budget checks | [resolved.rs:145 (per-body/stream size guard)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L145); [resolved.rs:374 (shared remaining-work counters)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L374); [resolved.rs:399 (every inflate, including replay)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L399); [resolved.rs:411 (every delta result)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L411) precede [parallel_index.rs:155 (try_reserve_exact)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/parallel_index.rs#L155). [read.rs:1009 (entry overflow)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/read.rs#L1009) and [read.rs:1196 (delta overflow)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/read.rs#L1196) reject lost high bits. | `declared_size_bomb_is_refused_before_allocation`; `delta_result_bomb_and_running_budgets_are_checked_before_visits`; `delta_commands_cannot_overrun_or_avoid_making_progress`; three replay-budget tests below | declared-object-size; declared-delta-result; declared-instructions; replayed-inflate-work; shared-replayed-result-work; compressed-replay-work; entry-size-overflow; delta-size-overflow; copy-output-pre-growth |
| 4. Delta depth ≤ 50 by default | Already handled on [parallel_index.rs:762 (origin/main@a71654f5: depth guard)](https://github.com/HeddleCo/sley/blob/a71654f50de24b3c750f0df5f1829db7342491f6/crates/sley-pack/src/parallel_index.rs#L762) and [parallel_index.rs:647 (checked before scheduling apply)](https://github.com/HeddleCo/sley/blob/a71654f50de24b3c750f0df5f1829db7342491f6/crates/sley-pack/src/parallel_index.rs#L647). Preserved in DFS at [resolved.rs:573 (check before replay/inflate/apply)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L573). Explicitly raised legacy depth limits remain supported. | `depth_51_is_refused_before_visiting_the_deep_object` | depth-before-apply |
| 5. sha1dc admission re-hash | Caller-owned by [#267 Design, “What the heddle side does in the callback”](https://github.com/HeddleCo/sley/issues/267). [resolved.rs:102 (public contract)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L102) specifies canonical header + body. Run sha1dc, compare to oid, reject collisions/mismatch at [resolved.rs:476 (visit(object))](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L476), before recording/admitting the object. Sley itself uses ordinary SHA-1/SHA-256. | `caller_admission_rejection_is_preserved_without_returning_a_build` tests verifier-error propagation, not the sha1dc algorithm. Its implementation/testing belongs to heddle. | caller-admission-boundary |
| 6. Thin, truncated, cancelled: no partial index | [resolved.rs:778 (unresolved completion refusal)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L778); [resolved.rs:167 (complete measured discovery before visits)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L167); [resolved.rs:355 (worker cancellation)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L355), [resolved.rs:655 (coordinator cancellation)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L655) and [resolved.rs:795 (final cancellation gate)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L795). Errors return no PackIndexBuild. Prior callback effects must stay provisional until Ok, as [resolved.rs:107 (documented)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/resolved.rs#L107). | `thin_pack_is_an_unresolved_base_error` (also a valid prefix + thin suffix); `truncation_and_trailing_bytes_are_errors`; `cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error` | thin-completion; truncated-discovery; cancel-polls |

The same private engine serves `resolve_pack`, the no-op indexing visitor and legacy external-base/progress entry points; see [parallel_index.rs:41 (compatibility wrapper)](https://github.com/HeddleCo/sley/blob/e8ca3f418bfc5c5f23038f942c9daeff79e3e505/crates/sley-pack/src/parallel_index.rs#L41). No speculative scanner remains in that path. External bases remain available only through the legacy APIs; the new visitor rejects thin packs.

## Self-attack

Let P = borrowed input length, N = checked object count, T = max_total_decoded, O = max_object_bytes, B = max_live_base_bytes, and W = min(requested worker budget, root count). Discovery has one 64 KiB scratch buffer. Compressed resolution work is capped at P + T; inflated-stream work and decoded whole/result work are each capped at T, **shared across all workers and including replay**. Logical discovery totals are checked independently before materialization. Ordered REF lookup, OFS binary search and final index sorting cost O(N log N); there is no every-offset inflate search.

B bounds retained ancestor bodies, **not process RSS**. Transient decoding additionally needs at most two bodies and one delta stream per worker. Historical allocation charges also cap aggregate logical body/stream allocations at 2T across workers. Metadata is O(N); allocator slack, fixed zlib/thread overhead and caller-retained output are separate. Configure N/O/T/W to fit the host; setting only B to heddle C-1689’s 512 MiB is not a total-process memory ceiling. The parallel regression uses a scaled 8 MiB pool with 16 workers.

| Attempted route | Result / evidence |
|---|---|
| Nested or adversarially ordered delta chains | Default 50-link guard runs before apply. Frames are heap DFS state, not recursive calls. `depth_51_is_refused_before_visiting_the_deep_object`; `forward_ref_chain_and_duplicate_oids_visit_each_entry`. Explicit higher caller limits are finite and capped by representable u16 depth. |
| Small live pool causes repeated large-base decoding | Remaining inflated/decoded work is charged before each allocation, including replay. `replay_work_cannot_exceed_the_shared_decoded_budget` refuses before a third large-root allocation. `replayed_delta_result_budget_is_shared_across_workers` forces 16 simultaneous roots: each subtree fits alone but their combined work must refuse. |
| Tiny decoded body, huge compressed stream of empty DEFLATE blocks | Decoded bytes alone are insufficient. Every replay also charges its compressed span against P + T. `low_output_deflate_replay_spends_encoded_work_budget` has 8,192 empty blocks, a one-byte base and 32 zero-result children; it refuses before re-inflating the base. |
| REF to a missing base, a rootless REF component, or many distinct missing IDs | No external resolver in the new API; completion refuses. One snapshot of the shrinking ordered REF map avoids repeated scans past missing IDs. `thin_pack_is_an_unresolved_base_error`; `cyclic_or_missing_offsets_and_ref_bases_never_produce_an_index`; `many_missing_ref_bases_have_linear_lookup_cpu_cost` uses 32,768 distinct missing IDs and a process-CPU bound. |
| OFS self-cycle, cycle through earlier entries, or offset into a header | Already protected by [read.rs:703 (origin/main: self-offset rejection)](https://github.com/HeddleCo/sley/blob/a71654f50de24b3c750f0df5f1829db7342491f6/crates/sley-pack/src/read.rs#L703); every other OFS link goes strictly backwards. DFS additionally requires an existing strictly earlier entry. `cyclic_or_missing_offsets_and_ref_bases_never_produce_an_index`. Removing both cycle guards still hits completion refusal; the cycle-specific diagnostic test goes RED. A rootless component is never recursively followed. |
| Huge instruction stream with tiny result | Header-declared stream size and running discovery work are checked before inflate, independently of result size. Both the 2^40 instruction declaration and an actual oversized instruction stream are exercised in `declared_size_bomb_is_refused_before_allocation` / `delta_commands_cannot_overrun_or_avoid_making_progress`. |
| Zero-size copy, zero opcode, out-of-range or truncated copy/insert, too much/too little output | Already bounded on [read.rs:1102 (origin/main: check before extending output)](https://github.com/HeddleCo/sley/blob/a71654f50de24b3c750f0df5f1829db7342491f6/crates/sley-pack/src/read.rs#L1102) and [read.rs:1130 (cursor advances per opcode)](https://github.com/HeddleCo/sley/blob/a71654f50de24b3c750f0df5f1829db7342491f6/crates/sley-pack/src/read.rs#L1130). A zero copy size means 65,536 bytes, not zero work; reserved opcode zero is rejected. `delta_commands_cannot_overrun_or_avoid_making_progress` checks all cases, including 4,096 copies against a declared zero-byte result with a peak-memory bound. Invalid deltas never reach the visitor. |
| Header/result sizes overflow u64 or usize | New high-bit checks reject shifted-out bits, then usize conversion and remaining-budget checks precede reservation. `overflowing_entry_and_delta_size_varints_are_refused` covers an entry size and both delta-prefix sizes; separate entry/delta guard-reverts go RED. Existing checked-add/range guards remain in the common walker. |
| Many empty/small objects evade byte limits | max_objects is checked before metadata reservation; available input also bounds the count. `many_empty_objects_are_bounded_before_metadata_allocation` rejects 8,192 entries under a 4,096 cap with no visits and a peak-memory assertion, then accepts all entries with zero decoded-byte budget when count permits them. The physical empty entry comes from the real writer. |
| Threads multiply independent caches or work budgets | There is one Engine/State and one atomic retained-byte gate, not one per worker. Parallel live-base peak and 16-worker replay-result tests prove the shared ceilings. Transient/stack overhead still scales with the caller’s explicit W; resource failure from fallible spawn_scoped returns an error. Threads are not selected by pack bytes beyond the bounded number of roots. |
| Duplicate OIDs, forward REF bases, or two workers claim the same child | Atomic root cursor claims once; OFS child vectors are taken once and REF lists removed once under the shared lock. Every descriptor has one base kind. Forward-REF/duplicate-OID test and real SHA-1/SHA-256 fixtures assert one visit per physical offset and independently apply instructions to reproduce each body. |
| Corrupt/truncated zlib, checksum, trailing bytes or cancellation during work/finish | Full discovery precedes visits; invalid streams/count/trailer refuse. Worker/coordinator/final cancellation gates return Err, never a partial index. The thin test explicitly exercises valid provisional prefix visits followed by refusal. A sort already in progress is bounded by N and cancellation is checked before return; callbacks are not transactional and their effects must be staged. |
| SHA-1 collision, wrong caller hash, callback error, or callback retaining unbounded data | Sley’s ordinary hash is not collision detection. The issue explicitly assigns sha1dc over canonical header plus body, oid comparison and admission to heddle’s callback. Caller rejection is preserved and cancels workers. Callback-owned retention/spooling, application-object validation and arbitrary callback CPU are caller responsibilities, outside the base gate. The test models an admission error; it does not claim to test the external sha1dc algorithm. |
| Wrappers, re-exports, second engine caller, mutable input or unsafe bypass | All public pack-index builders and materialized PackFile parsing route through the same private resolver; original-index matrix tests include the legacy wrapper. Descriptor/Engine/allocation helpers are not public. Safe Rust cannot mutate the borrowed pack concurrently or retain callback borrows beyond the call. Legacy targeted read APIs are separate APIs, not an alternate path inside resolve_pack; their bounded decoder has its own existing contract tests. Fully materialized PackFile intentionally retains requested output through its visitor and is not the bounded visitor interface. |

## Test evidence

All guard probes call public APIs. Every probe produced an **executed failing test (exit 101)** rather than a compile failure; source was restored afterwards. The speculative-probing probe temporarily restored the actual original `scan_candidate_range` and its helpers before discovery; no artificial spin loop was used. CPU tests run in isolated Linux child processes, measure process user + system CPU ticks, and have an eight-second cooperative watchdog. Peak tests measure VmHWM and, for the result reservation bomb, VmPeak.

The result-bomb test isolates max_object_bytes by setting the separate running cap to u64::MAX. With the object guard present it rejects in discovery with zero virtual growth; without it the allocator/worker path raises virtual peak and the assertion fails. The test never requires a 1 TiB allocation to succeed or be physically committed.

Compiler progress, duplicate subprocess reports and large returned index byte arrays are omitted from the excerpts below.

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

## Verification

All seven requested commands passed. Summing Cargo’s 224 successful binary/doc-test summaries gives **3,806 passed, 0 failed, 2 ignored**. Clippy passed locally; the unrelated base audit/lint lane remains #269. Documentation ran with warnings denied.

| Command | Exit | Elapsed |
|---|---:|---:|
| `cargo build --locked --workspace --tests` | 0 | 55.04 s |
| `cargo fmt --all --check` | 0 | 3.23 s |
| `python3 .github/workflows/scripts/clippy-panic-gate.py` | 0 | 4.34 s |
| `cargo clippy --locked --workspace --all-targets --all-features --no-deps -- -D warnings` | 0 | 12.39 s |
| `cargo test --locked --workspace` | 0 | 129.13 s |
| `cargo doc --locked --workspace --no-deps --lib` | 0 | 8.86 s |
| `bash scripts/check-publish-pipeline.sh` | 0 | 0.07 s |

Selected actual restored public API output:

```text
test cyclic_or_missing_offsets_and_ref_bases_never_produce_an_index ... ok
bomb: peak=5427200 bytes, growth=729088 bytes
test declared_size_bomb_is_refused_before_allocation ... ok
delta-result bomb: peak=5414912, growth=1409024
delta-result bomb: peak virtual growth=0
test delta_result_bomb_and_running_budgets_are_checked_before_visits ... ok
test overflowing_entry_and_delta_size_varints_are_refused ... ok
test truncation_and_trailing_bytes_are_errors ... ok
test caller_admission_rejection_is_preserved_without_returning_a_build ... ok
test forward_ref_chain_and_duplicate_oids_visit_each_entry ... ok
test thin_pack_is_an_unresolved_base_error ... ok
test cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error ... ok
test low_output_deflate_replay_spends_encoded_work_budget ... ok
test replay_work_cannot_exceed_the_shared_decoded_budget ... ok
copy overrun: peak=5902336, growth=1728512
test delta_commands_cannot_overrun_or_avoid_making_progress ... ok
test depth_51_is_refused_before_visiting_the_deep_object ... ok
test many_empty_objects_are_bounded_before_metadata_allocation ... ok
test replayed_delta_result_budget_is_shared_across_workers ... ok
distinct missing REF bases: CPU ticks=28, bound=400
test many_missing_ref_bases_have_linear_lookup_cpu_cost ... ok
crafted nested-member pack: CPU ticks=37, bound=400
test embedded_zlib_members_have_linear_discovery_cpu_cost ... ok
test real_packs_match_original_indexes_and_visit_once_with_reproducible_instructions ... ok
last-dependent release: peak=8822784 bytes
test last_dependent_releases_base_before_visiting_child ... ok
live bases: baseline=6205440, peak=58576896, cap=8388608 bytes (16 workers)
test live_base_bound_is_shared_and_replay_stays_within_peak_memory ... ok
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.70s
```

Pure-Rust backend:

```text
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.34s
```

Selected final gate output:

```text
Finished `dev` profile [unoptimized + debuginfo] target(s) in 54.90s
Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.22s
Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.83s
ok: explicit CRATES=( ... ) list present
ok: workflow reads secrets.CRATES_IO_API_KEY
ok: CARGO_REGISTRY_TOKEN env var declared
ok: validator self-test: correct passes; drop / bogus / mis-order / dup all fail
ok: CRATES matches the publishable set exactly (31 crates)
ok: CRATES is topologically ordered (149 in-set dependency edges)
publish-pipeline check passed
```

The first workspace attempt stopped in the existing MIDX fixture setup; it passed twice in isolation (including with CI identities) and the unchanged full workspace passed on retry. No MIDX test or unrelated lint was edited:

```text
thread 'multi_pack_index_expire_removes_repacked_packs' (55533) panicked at crates/sley-cli/tests/multi_pack_index.rs:659:9:
assertion `left == right` failed: setup should leave five packs
  left: 7
 right: 5
test result: FAILED. 8 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.25s
```

The parent contract’s export lint is unavailable in this repository (outside the seven Sley commands):

```text
error: no such command: `xtask`
```

## Production budget

Relative to `a71654f5`, excluding tests, fixtures and review evidence. Gross means additions **and** deletions, not only the net deletion credit.

| Production file | Added | Deleted | Net |
|---|---:|---:|---:|
| `crates/sley-pack/src/lib.rs` | 2 | 0 | +2 |
| `crates/sley-pack/src/parallel_index.rs` | 59 | 754 | -695 |
| `crates/sley-pack/src/read.rs` | 2 | 0 | +2 |
| `crates/sley-pack/src/resolved.rs` | 809 | 0 | +809 |
| **Total** | **872** | **754** | **+118** |

**Gross production churn: 1,626 lines (872 added, 754 deleted). Net: +118 / +420 target; below +525 stop.** Round 1 was +803/-754 = +49. Round 2 adds +69 net production: +81/-12 relative to `66f2701a`.

Workspace version remains 0.13.0. Cargo.lock, release-please configuration and unrelated lints are unchanged. Base audit/lint remediation is tracked separately by #269. This PR stays draft; no publish or merge.

