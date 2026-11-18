## Summary

- Restore parallel discovery for explicitly unlimited trusted indexing; skip its work mutex, operation charges and in-flight gate. Bounded callers keep sequential discovery and shared budgets.
- Type count, object-byte, decoded-work, workspace and legacy-operation errors; isolate trusted interrupt flags; restore Inflate/DeltaResolution spans and intermediate progress.
- Reduce and account metadata, share the physical preallocation bound, and prove the limits through public API guard reversions. Cumulative production is +610 against main, below the owner's revised +650 stop.

## Closes

Closes #267
Closes #259

## Test evidence

Round 5 resumed under the owner's 2026-11-18 +650 ruling. Main `6ec35770` was merged without conflicts by plain merge `7723e934`; its dependency/lockfile changes are inherited. No versions or release configuration were changed. Workspace 0.13.0 matches the example =0.13.0 pin. No PR merge, approval, closure or publication.

Source links and the final timing comparison use **0ba69878**. Later evidence-only commits leave its production tree unchanged. The initial +525 stop and failure are preserved in [the historical stop report](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/docs/SLEY_267_ROUND5_STOP.md); the owner superseded that stop. Current behavior and evidence follow.

### Original coverage audit

| Design defect | Fix in this branch | Public API test / reverted guard proof |
|---|---|---|
| Unbounded retained bases, including parallel resolution | [resolved.rs:789](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L789), [resolved.rs:802](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L802), [resolved.rs:942](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L942): one shared live counter, nearest retained ancestor replay, last-dependent release | `live_base_bound_is_shared_and_replay_stays_within_peak_memory`; `last_dependent_releases_base_before_visiting_child`; current GREEN below. Original live/release REDs remain in the linked round-4 report. |
| Superlinear every-offset zlib probing | [resolved.rs:375](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L375), [resolved.rs:416](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L416), [resolved.rs:478](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L478): speculation reachable only with all six resource limits unlimited | `embedded_zlib_members_have_linear_discovery_cpu_cost`; **untrusted-speculation RED rerun below**, plus finite-policy isolation. |
| Up-front declared body/instruction/result allocation | [resolved.rs:276](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L276), [resolved.rs:283](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L283), [resolved.rs:758](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L758), [resolved.rs:773](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L773), [parallel_index.rs:167](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/parallel_index.rs#L167) | `declared_size_bomb_is_refused_before_allocation`; `delta_result_bomb_and_running_budgets_are_checked_before_visits`; typed-budget and workspace REDs below. 2^40 peak assertions remain enabled. |
| Depth >50 before apply | [resolved.rs:923](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L923), [parallel_index.rs:155](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/parallel_index.rs#L155) | `depth_51_is_refused_before_visiting_the_deep_object`; GREEN below, original depth guard RED in linked round-4 evidence. |
| SHA-1 collision detection / caller contract | [resolved.rs:116](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L116), [index.rs:12](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/index.rs#L12); [#267 Design](https://github.com/HeddleCo/sley/issues/267) assigns sha1dc re-hashing to heddle's admission callback | `caller_admission_rejection_is_preserved_without_returning_a_build`; caller-verifier-error RED; both rendered public-field docs RED/GREEN below. Plain SHA-1 is never represented as sha1dc verification. |
| Thin / truncated / cancelled packs, no returned partial index | [resolved.rs:186](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L186), [resolved.rs:1209](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L1209), [resolved.rs:225](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L225) | `thin_pack_is_an_unresolved_base_error`; `truncation_and_trailing_bytes_are_errors`; `cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error`; thin-completion / caller-error REDs below. Callback effects are explicitly provisional until Ok and must be staged. |
| Byte-identical index / exactly-once visits / reproducible instructions | [parallel_index.rs:59](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/parallel_index.rs#L59): one generic engine with a no-op visitor | `real_packs_match_original_indexes_and_visit_once_with_reproducible_instructions` checks original SHA-1/SHA-256 fixtures at threads 1,2,16 and three live-base policies, plus the compatibility wrapper. Both large timing fixtures also match base idx SHA-256 digests. |

### Round-4 findings retained and re-proved

| Finding | Fix | RED test/guard rerun |
|---|---|---|
| Deprecated fetch_update broke warnings-denied CI | [resolved.rs:789](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L789): try_update | deprecated-atomic (expected compile RED) |
| Legacy callers accidentally inherited untrusted caps | [parallel_index.rs:19](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/parallel_index.rs#L19), [parallel_index.rs:59](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/parallel_index.rs#L59), [parallel_index.rs:85](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/parallel_index.rs#L85): unlimited defaults, explicit bounded options | trusted-default-policy; legacy-index-route; legacy-parse-route |
| Working buffers multiplied outside shared gate | [resolved.rs:718](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L718), [resolved.rs:877](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L877), [resolved.rs:966](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L966): complete workspace lease and bounded actual workers | shared-flight-gates; worker-arena-bound; flight-lease-lifetime; flight-error-release; delta-stream-workspace; replay-base-workspace |
| Replay amplified inflate/apply operation count | [resolved.rs:94](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L94), [resolved.rs:708](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L708), [resolved.rs:452](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L452): every discovery/resolve/replay inflate and apply counts | resolve-op-ceiling; discovery-op-charge; inflate-op-charge; apply-op-charge |
| Unparsed count reserved excessive metadata | [limits.rs:62](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/limits.rs#L62), [resolved.rs:400](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L400): same byte/count bound on both policies | bounded-preallocation; physical-preallocation-bound |
| Public SHA-1 contract unclear | public oid/idx field docs, links above | oid-docs; idx-docs, rendered-doc RED/GREEN |
| Late thin-pack callback effects | explicit provisional callback contract and completion checks | thin-completion; caller-verifier-error |
| Linux-only helper compiled on Windows | [resolved.rs:661](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/tests/resolved.rs#L661): existing enclosing Linux fence retained | committed Windows cfg probe removes fence, requires compile RED, restores it before public API GREEN; hosted evidence appended below |

### Round-5 findings

| Finding | Fix | Public API RED proof |
|---|---|---|
| Trusted serial discovery / per-op mutex and counter cost | [resolved.rs:50](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L50), [resolved.rs:478](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L478), [resolved.rs:683](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L683), [resolved.rs:708](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L708), [resolved.rs:877](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L877) | interleaved base/head timings below; limited-fast-path-isolation and untrusted-speculation REDs prevent expanding the fast policy |
| max_objects error was a string | [resolved.rs:394](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L394), new [lib.rs:1736](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-core/src/lib.rs#L1736) kind; ResolveError non-exhaustive | `object_count_budget_is_typed` / typed-object_count_budget_is_typed |
| max_object_bytes error was a string | [resolved.rs:276](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L276) (also checks usize representation) | `object_bytes_budget_is_typed` / typed-object_bytes_budget_is_typed |
| max_total_decoded error was a string | [resolved.rs:283](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L283) | `decoded_bytes_budget_is_typed` / typed-decoded_bytes_budget_is_typed |
| Workspace refusal / arithmetic overflow was a string | [resolved.rs:1063](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L1063), [resolved.rs:718](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L718) | `workspace_fit_budget_is_typed`; `workspace_arithmetic_overflow_stays_a_typed_budget_error` / typed-workspace_fit_budget_is_typed; workspace-overflow-typing |
| Legacy adapters erased ResolveOps typing | [resolved.rs:164](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L164), [parallel_index.rs:120](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/parallel_index.rs#L120) | `legacy_operation_budget_remains_typed` / legacy-ops-index-typing; legacy-ops-parse-typing |
| Excess per-object metadata, C-1712 | [resolved.rs:578](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L578), [resolved.rs:957](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L957), [resolved.rs:1105](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L1105), [resolved.rs:1213](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L1213), [resolved.rs:1235](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L1235), [resolved.rs:87](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L87) | `million_objects_fit_metadata_coefficient` at 1M/4M; `ref_slots_and_pending_snapshot_are_in_metadata_accounting`; original-metadata-layout RED |
| Workspace sizing condition undocumented | [resolved.rs:198](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L198): gate must fit 2×largest result plus largest delta stream; 3×max_object_bytes suffices. Actual per-pack maxima determine admission/worker count | workspace/base/instruction REDs above; typed fit/overflow tests |
| Trusted errors set process interrupt | [resolved.rs:225](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L225), [resolved.rs:673](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L673): only visitor API boundary sets caller source; private engine stops its own workers | `trusted_errors_leave_process_interrupt_clear` / trusted-cancel-isolation; checksum, depth and thin failures all covered |
| Inflate / DeltaResolution spans missing | [resolved.rs:415](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L415), [resolved.rs:777](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L777) | `trusted_index_restores_inflate_and_delta_profile_spans` / profile-Inflate; profile-DeltaResolution |
| Coalesced progress skipped intermediate counts on small packs | [resolved.rs:1013](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/src/resolved.rs#L1013) | `trusted_small_packs_keep_intermediate_progress` / small-pack-progress; transport `progress_installer_reports_monotonic_transfer` now GREEN |

Budget mapping uses enum variants, never strings: `ResolveObjects → max_objects`, `ResolveObjectBytes → max_object_bytes`, `ResolveTotalDecoded → max_total_decoded`, `ResolveInFlight → in_flight_bytes`, and `ResolveOps → resolve_ops`. Public visitor operation exhaustion remains ResolveError::ResolveOps; legacy adapters preserve ResourceLimitKind::ResolveOps. All allocation/work refusals retain their numeric limit/attempted fields (overflow attempts saturate at u64::MAX).

### Trusted timing: base 61848386 versus final production source 0ba69878

Release builds of the same public API harness, 16 requested threads, the same 16-CPU affinity, input already read before timing, no local Cargo/proof workload running during timed trials. Order is base/head repeated three times per fixture; no best-run selection. Absolute times varied with host load, so every sample and the required median are shown. Source exports use git archive; no branch or worktree was created.

Both fixtures contain **420,000 objects**. Whole: **378,420,032 bytes**. Mixed: **374,010,032 bytes**, 210,000 whole roots and 210,000 OFS copy/insert delta children. Reproduction: [benchmark driver](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/tests/support/resolve_timing.py), [public API harness](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/tests/support/resolve_timing.rs).

| Fixture | Version | Run 1 (s) | Run 2 (s) | Run 3 (s) | Median (s) | Head/base |
|---|---|---:|---:|---:|---:|---:|
| whole | base | 3.685745 | 4.003410 | 3.311393 | 3.685745 | — |
| whole | head | 3.171434 | 3.301696 | 3.471582 | 3.301696 | 0.8958× |
| mixed-ofs | base | 3.517128 | 2.605209 | 2.311598 | 2.605209 | — |
| mixed-ofs | head | 2.715155 | 2.499176 | 2.451220 | 2.499176 | 0.9593× |

Both median gates pass **head ≤ 1.05×base**. All six runs per fixture returned the same idx SHA-256:

```text
whole:     a5155d18d8f3ef834a458bc68f5176071758e5ff171fcb346163816e6ff6b571
mixed-ofs: 38afa8753433f213b82a0b905081a9d873fda931b2c9f1ce8f359df9476e2179
```

### Metadata and C-1709 RSS sum

The portable meter conservatively counts descriptor/output/root/child vectors, sorted REF slots and link capacities (including append overlap), grouping input, pending snapshot, returned entries/objects, index capacity and sorting/large-offset scratch. Fixed worker/zlib state and allocator overhead are separate. The no-op visitor retains nothing. The Linux test runs in an isolated child and permits a stated 64 MiB fixed slack over its input-allocated baseline.

| Objects / shape | Accounted peak | Accounted B/object | Linux RSS growth | RSS B/object |
|---|---:|---:|---:|---:|
| 1,000,000 empty whole entries | 232,000,000 | 232.000 | 233,623,552 | 233.624 |
| 4,000,000 empty whole entries | 928,000,000 | 232.000 | 929,452,032 | 232.363 |
| 32,768 distinct missing REF bases | 8,421,376 | 257.000 | not separately measured | — |

C-1712 keeps **300 B/object**: the measured floor is below 300, so the owner's above-300 adjustment is unnecessary. The portable assertion is accounted ≤300×N; Linux additionally asserts RSS growth ≤300×N +64 MiB. Restoring the original root/output layouts and root lifetime produces **346.332 B/object** and the unchanged portable coefficient assertion fails. The proof also updates bookkeeping to count the restored live root vector; it does not weaken the coefficient.

For a 4 GiB import process, the following explicit C-1709 envelope fits its 3.5 GiB target:

| Component | Allowance |
|---|---:|
| Shared in-flight buffers | 1 GiB |
| Shared retained bases | 512 MiB |
| Input pack, caller-enforced | 512 MiB |
| Caller-owned staged materialization | 256 MiB |
| Worker/thread/zlib state | 64 MiB |
| Allocator and other fixed slack | 64 MiB |
| Metadata with max_objects=4,000,000 | 1,200,000,000 B |
| **Total** | **3,750,136,832 B = 3.492587 GiB** |

Therefore name **max_objects=4,000,000**, at most 16 workers and the stated input/staging bounds for this envelope; the library's generic 10M default does not itself promise a 4 GiB process. Margin to 3.5 GiB is 7.591 MiB. For other input/staging/fixed reservations, choose N ≤floor((3.5 GiB−F−B−P−staging−fixed)/300). Materializing PackFile output is outside the resolver gates, as documented; heddle uses the bounded visitor and its own staging budget.

### Self-attack, including unlimited fast path

| Route | Why bounded / verification |
|---|---|
| Nested OFS/REF deltas | Depth checked before apply, default 50. Every replay inflate/apply spends shared byte and operation budgets; deep-chain and replay tests. |
| Missing REF base / thin pack | Public visitor hardcodes no external callback. Sorted REF slots and one pending snapshot avoid quadratic scans; thin/missing-REF CPU tests. All callback effects remain provisional until Ok. |
| OFS self/future/cyclic references | Format distance and earlier-entry lookup reject cycles; REF-only cycles stay unresolved and return no build. `cyclic_or_missing_offsets_and_ref_bases_never_produce_an_index`. |
| Huge declared whole / instructions / delta-result sizes | Per-object and logical work checks before declared-size allocation, including every replay. 2^40 RSS/VmPeak assertions; entry/delta varint overflow tests. |
| Huge instruction counts / tiny commands | Instruction bytes spend object/inflated/compressed budgets; result preflight and exact application bound command output. Operation budget counts every apply, including replay. `delta_commands_cannot_overrun_or_avoid_making_progress`. |
| Zero-length copy / zero-progress opcode | Git's encoded zero copy size means 65,536, checked against base/result; opcode zero is invalid. Existing checked delta walker; copy/insert conformance and bounds tests. |
| Millions of tiny/empty objects | max_objects checked before metadata allocation; common physical bound min(count,entry_bytes/9,65536) before initial reserve. Growth tied to parsed entries. Empty objects still spend operations; 1M/4M metadata and count tests. |
| False zlib members nested inside a blob | Limited discovery follows only actual entry boundaries. CPU test GREEN; switching that branch to speculative scan consumes 785 ticks against a 400-tick ceiling and goes RED. |
| Threads multiply transient bytes | One shared full-workspace gate, admission before any walk buffer, actual worker count also fits gate; 16-root pressure counter + Linux RSS. |
| Buffer omitted from gate | Workspace covers replay old body + instructions + newly allocated result. Base/instruction omission and early-lease-drop probes all RED; discovery scratch is fixed and separately documented. |
| Charge released before buffer / on error | Flight lease outlives walk buffers; Retained frees Vec before decrementing live bytes. Error/cancel cleanup test asserts zero outstanding flight bytes; cleanup mutant RED. |
| Retained bases never released | Shared live gate, nearest retained ancestor fallback, release before last-child callback. Live-base and last-dependent memory tests. |
| Replay CPU evades byte budgets with tiny steps | Every discovery inflate plus every resolve/replay inflate/apply counts. 1M-leaf/depth-50 crafted pack stops at limit 1,010,051, before walking the leaves; uncharged mutant reaches ~14M operations before watchdog cancellation. |
| Empty DEFLATE blocks consume CPU without output | Compressed work is bounded by input length plus decoded work cap; `low_output_deflate_replay_spends_encoded_work_budget`. |
| Metadata hidden in REF map / pending snapshot / index temporaries | Sorted vector slots replace opaque tree nodes; capacities and construction/conversion/index overlap are accounted. REF-heavy test, 1M/4M portable + RSS tests, original-layout RED. |
| Integer arithmetic exceeds representable workspace | Checked additions and usize conversion return typed ResolveInFlight (or platform object-byte refusal), before decoding. Workspace-overflow RED/GREEN. |
| Finite caller accidentally enters unlimited discovery | is_unlimited compares ALL six resource fields; one finite field keeps sequential discovery and operation/flight accounting. Each field, threads 0/1/2/16, both builder orders, Default and PackIndexOptions::new are tested. |
| Default options / builder / threads override loses limits | ResolveLimits::default is bounded. Legacy PackIndexOptions defaults/new are intentionally trusted per C-1709; an untrusted caller must use resolve_pack/default limits or with_resolve_limits. Once opted in, with_threads and builder order preserve every cap. Builder fields holding the policy are private. Explicit with_resolve_limits(unlimited()) is an explicit trusted policy, not a bypass. |
| Unlimited path has no byte/CPU caps | Deliberate only for trusted callers per C-1709; speculative discovery, unlimited body sizes/work and materialized output are not an untrusted API promise. No finite resource field reaches it; isolation and speculation guard reversions fail. Depth checks and cancellation still run. |
| Unverified SHA-1 object | Oid/idx docs explicitly say plain SHA-1. Caller sha1dc canonical re-hash/digest comparison is required by #267 Design. Visitor verifier errors propagate and no build is returned; staged writes discarded on error. |
| Caller retains bodies or runs unbounded callback work | Outside the engine's borrowed-buffer contract; caller owns staging, CPU and output limits. PackFile materialization is output storage; bounded import uses the visitor. |
| Trusted failure changes process-wide cancellation | Private worker stop is separate; only public visitor boundary sets caller source. Corrupt/checksum/depth/thin tests and restored-old-cancel RED. |

### Guard-reverted RED output

Every behavior probe executes through public API; no implementation-mirroring unit seam. The deprecated atomic is an expected warnings-denied compile RED. All mutations restore originals in finally. The committed [proof driver](https://github.com/HeddleCo/sley/blob/0ba698787b2c2e074851b72bb47baf74d710fde6/crates/sley-pack/tests/support/resolve_red_proofs.py) reproduces the 32 guards. Full rerun was on 0ba's predecessor with identical guarding logic; the final metadata accounting extension was additionally re-proved after its change. Public GREEN below is on the final production tree. The two field documentation probes and hosted Windows fence probe are additional checks.

The unparsed-count vector was enlarged from 3 MB to 9 MB so it still reaches preallocation after the format's true nine-byte minimum replaced the prior loose three-byte validation. The new one-KiB count vector uses u32::MAX (2^32−1), the largest value the four-byte count can encode. It asserts both ≤8 MiB and a stronger ≤16 KiB physical-input allowance. Removing only the byte bound while retaining the 64K cap allocates 5,767,168 B and fails that stronger assertion; the test cannot pass vacuously behind the old count cap.

#### RED: deprecated-atomic

Test: `real_packs_match_original_indexes_and_visit_once_with_reproducible_instructions`; exit 101.

```text
error: use of deprecated method `std::sync::atomic::Atomic::<usize>::fetch_update`: renamed to `try_update` for consistency
help: replace the use of the deprecated method
```

#### RED: trusted-default-policy

Test: `trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply`; exit 101.

```text
thread 'trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply' (355) panicked at crates/sley-pack/tests/resolved.rs:1230:5:
assertion `left == right` failed
  left: 17179869184
 right: 18446744073709551615
test trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
```

#### RED: legacy-index-route

Test: `trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply`; exit 101.

```text
thread 'trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply' (692) panicked at crates/sley-pack/tests/resolved.rs:1257:34:
test trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
```

#### RED: legacy-parse-route

Test: `trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply`; exit 101.

```text
thread 'trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply' (1029) panicked at crates/sley-pack/tests/resolved.rs:1262:58:
test trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.02s
```

#### RED: shared-flight-gates

Test: `in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory`; exit 101.

```text
flight: peak=268435680, gate=25165824, after=0
flight RSS: peak=285888512, gate+live+64MiB=100663296
test in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 7.36s
thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (1374) panicked at crates/sley-pack/tests/resolved.rs:373:5:
thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (1376) panicked at crates/sley-pack/tests/resolved.rs:1324:9:
in-flight working memory exceeded RSS bound
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 7.38s
```

#### RED: worker-arena-bound

Test: `in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory`; exit 101.

```text
flight: peak=16777230, gate=25165824, after=0
flight RSS: peak=285749248, gate+live+64MiB=100663296
test in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 17.11s
thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (1729) panicked at crates/sley-pack/tests/resolved.rs:373:5:
thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (1731) panicked at crates/sley-pack/tests/resolved.rs:1324:9:
in-flight working memory exceeded RSS bound
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 17.14s
```

#### RED: flight-lease-lifetime

Test: `in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory`; exit 101.

```text
test in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 7.06s
thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (2084) panicked at crates/sley-pack/tests/resolved.rs:373:5:
thread '<unnamed>' (2087) panicked at crates/sley-pack/tests/resolved.rs:1299:13:
lease released while callback borrows buffers
thread 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory' (2086) panicked at crates/sley-pack/tests/resolved.rs:1311:6:
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 7.07s
```

#### RED: flight-error-release

Test: `flight_reservations_release_after_errors_and_waiters_observe_cancel`; exit 101.

```text
thread 'flight_reservations_release_after_errors_and_waiters_observe_cancel' (2424) panicked at crates/sley-pack/tests/resolved.rs:1373:9:
assertion `left == right` failed: all workspace leases must drop on error
  left: 65548
 right: 0
test flight_reservations_release_after_errors_and_waiters_observe_cancel ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.04s
```

#### RED: delta-stream-workspace

Test: `flight_reservations_release_after_errors_and_waiters_observe_cancel`; exit 101.

```text
thread 'flight_reservations_release_after_errors_and_waiters_observe_cancel' (2762) panicked at crates/sley-pack/tests/resolved.rs:1410:6:
test flight_reservations_release_after_errors_and_waiters_observe_cancel ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.13s
```

#### RED: replay-base-workspace

Test: `flight_reservations_release_after_errors_and_waiters_observe_cancel`; exit 101.

```text
thread 'flight_reservations_release_after_errors_and_waiters_observe_cancel' (3103) panicked at crates/sley-pack/tests/resolved.rs:1371:9:
assertion `left == right` failed
  left: 2
 right: 1
test flight_reservations_release_after_errors_and_waiters_observe_cancel ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.04s
```

#### RED: resolve-op-ceiling

Test: `million_tiny_leaves_stop_at_resolve_op_budget`; exit 101.

```text
tiny replay: pack=16867462 bytes, objects=1000051, ops=14021969, limit=1010051, visits=130244
thread 'million_tiny_leaves_stop_at_resolve_op_budget' (3442) panicked at crates/sley-pack/tests/resolved.rs:1581:5:
test million_tiny_leaves_stop_at_resolve_op_budget ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 30.67s
```

#### RED: discovery-op-charge

Test: `resolve_ops_counts_discovery_inflate_apply_and_replay`; exit 101.

```text
thread 'resolve_ops_counts_discovery_inflate_apply_and_replay' (3781) panicked at crates/sley-pack/tests/resolved.rs:1438:9:
assertion `left == right` failed
  left: 5
 right: 8
test resolve_ops_counts_discovery_inflate_apply_and_replay ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.01s
```

#### RED: inflate-op-charge

Test: `resolve_ops_counts_discovery_inflate_apply_and_replay`; exit 101.

```text
thread 'resolve_ops_counts_discovery_inflate_apply_and_replay' (4119) panicked at crates/sley-pack/tests/resolved.rs:1438:9:
assertion `left == right` failed
  left: 5
 right: 8
test resolve_ops_counts_discovery_inflate_apply_and_replay ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.01s
```

#### RED: apply-op-charge

Test: `resolve_ops_counts_discovery_inflate_apply_and_replay`; exit 101.

```text
thread 'resolve_ops_counts_discovery_inflate_apply_and_replay' (4457) panicked at crates/sley-pack/tests/resolved.rs:1438:9:
assertion `left == right` failed
  left: 6
 right: 8
test resolve_ops_counts_discovery_inflate_apply_and_replay ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.01s
```

#### RED: bounded-preallocation

Test: `declared_count_preallocation_is_capped_before_parsing`; exit 101.

```text
declared count: virtual peak growth=38907904, bound=8388608
test declared_count_preallocation_is_capped_before_parsing ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.57s
thread 'declared_count_preallocation_is_capped_before_parsing' (4795) panicked at crates/sley-pack/tests/resolved.rs:373:5:
thread 'declared_count_preallocation_is_capped_before_parsing' (4797) panicked at crates/sley-pack/tests/resolved.rs:1612:9:
reservation followed unparsed header count
```

#### RED: thin-completion

Test: `thin_pack_is_an_unresolved_base_error`; exit 101.

```text
thread 'thin_pack_is_an_unresolved_base_error' (5134) panicked at crates/sley-pack/tests/resolved.rs:122:10:
test thin_pack_is_an_unresolved_base_error ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
```

#### RED: caller-verifier-error

Test: `caller_admission_rejection_is_preserved_without_returning_a_build`; exit 101.

```text
thread 'caller_admission_rejection_is_preserved_without_returning_a_build' (5471) panicked at crates/sley-pack/tests/resolved.rs:987:5:
test caller_admission_rejection_is_preserved_without_returning_a_build ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.01s
assertion failed: matches!(result, Err(ResolveError::Visitor(AdmissionRejected)))
```

#### RED: physical-preallocation-bound

Test: `lying_maximum_object_count_over_one_kib_has_bounded_allocation`; exit 101.

```text
lying count: preallocated=5767168 bytes, ceiling=8388608
test lying_maximum_object_count_over_one_kib_has_bounded_allocation ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
thread 'lying_maximum_object_count_over_one_kib_has_bounded_allocation' (5809) panicked at crates/sley-pack/tests/resolved.rs:373:5:
thread 'lying_maximum_object_count_over_one_kib_has_bounded_allocation' (5811) panicked at crates/sley-pack/tests/resolved.rs:1655:9:
reservation must follow the one-KiB input, not just a fixed count cap
```

#### RED: typed-object_count_budget_is_typed

Test: `object_count_budget_is_typed`; exit 101.

```text
thread 'object_count_budget_is_typed' (6148) panicked at crates/sley-pack/tests/resolved.rs:1675:5:
wrong budget variant: Pack(InvalidFormat("max_objects: limit 1, attempted 2"))
test object_count_budget_is_typed ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
```

#### RED: typed-object_bytes_budget_is_typed

Test: `object_bytes_budget_is_typed`; exit 101.

```text
thread 'object_bytes_budget_is_typed' (6485) panicked at crates/sley-pack/tests/resolved.rs:1675:5:
wrong budget variant: Pack(InvalidFormat("max_object_bytes: limit 31, attempted 32"))
test object_bytes_budget_is_typed ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
```

#### RED: typed-decoded_bytes_budget_is_typed

Test: `decoded_bytes_budget_is_typed`; exit 101.

```text
thread 'decoded_bytes_budget_is_typed' (6822) panicked at crates/sley-pack/tests/resolved.rs:1675:5:
wrong budget variant: Pack(InvalidFormat("max_total_decoded: limit 31, attempted 32"))
test decoded_bytes_budget_is_typed ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
```

#### RED: typed-workspace_fit_budget_is_typed

Test: `workspace_fit_budget_is_typed`; exit 101.

```text
thread 'workspace_fit_budget_is_typed' (7159) panicked at crates/sley-pack/tests/resolved.rs:1675:5:
wrong budget variant: Pack(InvalidFormat("max_in_flight_bytes workspace: limit 1, attempted 72"))
test workspace_fit_budget_is_typed ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.01s
```

#### RED: legacy-ops-index-typing

Test: `legacy_operation_budget_remains_typed`; exit 101.

```text
thread 'legacy_operation_budget_remains_typed' (7497) panicked at crates/sley-pack/tests/resolved.rs:1961:9:
test legacy_operation_budget_remains_typed ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
assertion failed: matches!(e, GitError::ResourceLimit
```

#### RED: legacy-ops-parse-typing

Test: `legacy_operation_budget_remains_typed`; exit 101.

```text
thread 'legacy_operation_budget_remains_typed' (7834) panicked at crates/sley-pack/tests/resolved.rs:1961:9:
test legacy_operation_budget_remains_typed ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
assertion failed: matches!(e, GitError::ResourceLimit
```

#### RED: trusted-cancel-isolation

Test: `trusted_errors_leave_process_interrupt_clear`; exit 101.

```text
thread 'trusted_errors_leave_process_interrupt_clear' (8171) panicked at crates/sley-pack/tests/resolved.rs:1741:9:
trusted pack error changed process-wide interrupt flag
test trusted_errors_leave_process_interrupt_clear ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.02s
```

#### RED: limited-fast-path-isolation

Test: `finite_limits_cannot_enter_unaccounted_trusted_discovery`; exit 101.

```text
thread 'finite_limits_cannot_enter_unaccounted_trusted_discovery' (8525) panicked at crates/sley-pack/tests/resolved.rs:1807:13:
finite policy bypassed operation accounting
test finite_limits_cannot_enter_unaccounted_trusted_discovery ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.02s
```

#### RED: original-metadata-layout

Test: `million_objects_fit_metadata_coefficient`; exit 101.

```text
metadata: objects=1000000, accounted=346331648, bytes/object=346.332, coefficient=300
test million_objects_fit_metadata_coefficient ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 11.75s
thread 'million_objects_fit_metadata_coefficient' (341) panicked at crates/sley-pack/tests/resolved.rs:373:5:
thread 'million_objects_fit_metadata_coefficient' (343) panicked at crates/sley-pack/tests/resolved.rs:1920:5:
per-object metadata exceeded coefficient
```

#### RED: profile-Inflate

Test: `trusted_index_restores_inflate_and_delta_profile_spans`; exit 101.

```text
thread 'trusted_index_restores_inflate_and_delta_profile_spans' (9207) panicked at crates/sley-pack/tests/resolved.rs:2023:9:
missing profile stage Inflate
test trusted_index_restores_inflate_and_delta_profile_spans ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 38 filtered out; finished in 0.02s
```

#### RED: profile-DeltaResolution

Test: `trusted_index_restores_inflate_and_delta_profile_spans`; exit 101.

```text
thread 'trusted_index_restores_inflate_and_delta_profile_spans' (9579) panicked at crates/sley-pack/tests/resolved.rs:2023:9:
missing profile stage DeltaResolution
test trusted_index_restores_inflate_and_delta_profile_spans ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 38 filtered out; finished in 0.02s
```

#### RED: small-pack-progress

Test: `trusted_small_packs_keep_intermediate_progress`; exit 101.

```text
thread 'trusted_small_packs_keep_intermediate_progress' (9948) panicked at crates/sley-pack/tests/resolved.rs:2047:13:
coalescing lost intermediate object progress
test trusted_small_packs_keep_intermediate_progress ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.01s
```

#### RED: untrusted-speculation

Test: `embedded_zlib_members_have_linear_discovery_cpu_cost`; exit 101.

```text
crafted nested-member pack: CPU ticks=785, bound=400
test embedded_zlib_members_have_linear_discovery_cpu_cost ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 8.27s
thread 'embedded_zlib_members_have_linear_discovery_cpu_cost' (10287) panicked at crates/sley-pack/tests/resolved.rs:373:5:
thread 'embedded_zlib_members_have_linear_discovery_cpu_cost' (10289) panicked at crates/sley-pack/tests/resolved.rs:1140:5:
```

#### RED: workspace-overflow-typing

Test: `workspace_arithmetic_overflow_stays_a_typed_budget_error`; exit 101.

```text
thread 'workspace_arithmetic_overflow_stays_a_typed_budget_error' (10629) panicked at crates/sley-pack/tests/resolved.rs:2097:5:
untyped workspace refusal: Pack(InvalidFormat("pack workspace size overflow"))
test workspace_arithmetic_overflow_stays_a_typed_budget_error ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
```

#### RED: oid-docs

```text
Traceback (most recent call last):
  File "/home/heddleco/HeddleCo/workspace/sley-267/crates/sley-pack/tests/public_docs.py", line 14, in <module>
    assert "sha1dc" in section, f"{page}::{field}: missing collision-detection warning"
           ^^^^^^^^^^^^^^^^^^^
AssertionError: resolved/struct.ResolvedObject.html::oid: missing collision-detection warning
```

#### RED: idx-docs

```text
public docs: resolved/struct.ResolvedObject.html::oid identifies plain SHA-1 and caller sha1dc requirement
Traceback (most recent call last):
  File "/home/heddleco/HeddleCo/workspace/sley-267/crates/sley-pack/tests/public_docs.py", line 14, in <module>
    assert "sha1dc" in section, f"{page}::{field}: missing collision-detection warning"
           ^^^^^^^^^^^^^^^^^^^
AssertionError: struct.PackIndexBuild.html::index: missing collision-detection warning
```

### Restored public API GREEN

```text
typed budget: ResolveTotalDecoded
legacy budget: index and parse preserve typed resolve_ops (limit=1, attempted=2)
delta-result bomb: peak=6311936, growth=2043904
delta-result bomb: peak virtual growth=0
bomb: peak=6381568 bytes, growth=1646592 bytes
lying count: preallocated=9680 bytes, ceiling=8388608
lying count: preallocated=9680 bytes, ceiling=8388608
typed budget: ResolveObjectBytes
typed budget: ResolveObjects
flight: visitor/cancel errors release all leases; oversized workspace refused before allocation
trusted errors: checksum, depth and thin failures preserve interrupt; public visitor sets cancel
workspace overflow: typed ResolveInFlight, refused before decoding
typed budget: ResolveInFlight
distinct missing REF bases: CPU ticks=13, bound=400
policy isolation: each finite field, threads 0/1/2/16, default/new builders and both builder orders accounted; explicit unlimited has zero accounting
trusted progress: intermediate samples preserved at threads 1/16
crafted nested-member pack: CPU ticks=46, bound=400
declared count: virtual peak growth=0, bound=8388608
last-dependent release: peak=9650176 bytes
tiny replay: pack=16867462 bytes, objects=1000051, ops=1010051, limit=1010051, visits=125
REF metadata: count=32768, accounted=8421376, bytes/object=257.000
metadata: objects=1000000, accounted=232000000, bytes/object=232.000, coefficient=300
metadata RSS: baseline=14057472, peak=247681024, growth=233623552, bytes/object=233.624, fixed slack=67108864
flight: peak=16777230, gate=25165824, after=0
flight RSS: peak=41152512, gate+live+64MiB=100663296
test result: ok. 38 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.83s
```

4M positive measurement, separate isolated public API run:

```text
metadata: objects=4000000, accounted=928000000, bytes/object=232.000, coefficient=300
metadata RSS: baseline=41230336, peak=970682368, growth=929452032, bytes/object=232.363, fixed slack=67108864
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 48.97s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 48.97s
```

### Local CI verification

Stable 1.100, locked dependency graph; all seven requested checks plus HTTP embedding, public API backends, profile and rendered-field documentation. No base lint or lockfile edit was used to obtain green. The first local run found one RangeInclusive style lint in the new test (corrected), then the runner build volume filled up. A fresh task-owned /tmp target completed every requested command successfully; no shared artifacts were deleted and cargo clean was not used.

| Check | Exit | Seconds |
|---|---:|---:|
| build | 0 | 83.5 |
| fmt | 0 | 3.65 |
| panic-gate | 0 | 9.82 |
| clippy | 0 | 35.39 |
| test | 0 | 149.19 |
| doc | 0 | 11.68 |
| publish-pipeline | 0 | 0.05 |
| public-api | 0 | 29.61 |
| pure-rust | 0 | 29.41 |
| profile | 0 | 6.6 |
| http-embedding | 0 | 71.95 |
| public-doc-contract | 0 | 0.03 |

Workspace totals: **3827 passed, 0 failed, 2 ignored**, 224 Cargo test summaries.

public-api:

```text
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.00s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.11s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.11s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.30s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.87s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 0.88s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 3.84s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 11.07s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 15.72s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 37 filtered out; finished in 22.82s
test result: ok. 38 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.83s
```

pure-rust:

```text
test result: ok. 38 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 24.23s
```

profile:

```text
fetch-profile: Inflate and DeltaResolution both recorded
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 38 filtered out; finished in 0.02s
```

http-embedding:

```text
test result: ok. 186 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.75s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

public-doc-contract:

```text
public docs: resolved/struct.ResolvedObject.html::oid identifies plain SHA-1 and caller sha1dc requirement
public docs: struct.PackIndexBuild.html::index identifies plain SHA-1 and caller sha1dc requirement
```

The earlier hosted d06a8ef4 run caught the small-pack progress regression; the subsequent guard-reverted public test and installer integration test both prove the correction. All final hosted check/step conclusions and Windows RED/GREEN are appended to the PR body after the pushed evidence head completes; the report does not predict results.

### Cumulative production budget

Against origin/main 6ec35770, excluding tests, fixtures, CI test wiring and evidence. The inherited main merge is excluded.

| File | Added | Deleted | Net |
|---|---:|---:|---:|
| `crates/sley-core/src/lib.rs` | 10 | 0 | +10 |
| `crates/sley-pack/src/index.rs` | 5 | 0 | +5 |
| `crates/sley-pack/src/lib.rs` | 2 | 0 | +2 |
| `crates/sley-pack/src/limits.rs` | 9 | 13 | -4 |
| `crates/sley-pack/src/parallel_index.rs` | 80 | 756 | -676 |
| `crates/sley-pack/src/read.rs` | 22 | 1 | +21 |
| `crates/sley-pack/src/resolved.rs` | 1252 | 0 | +1252 |
| **Total** | **1380** | **770** | **+610** |

Gross production churn: **2150 lines**. Cumulative **+610 / +650 stop**, +252 this round; +420 remains the original target. No budget band used.

Historical original-design guard output is preserved in [round 4](https://github.com/HeddleCo/sley/blob/170c8053503487baf1ffe0a8b48ab83fd63384e9/docs/SLEY_267_ROUND4_REVIEW.md) and [round 2](https://github.com/HeddleCo/sley/blob/170c8053503487baf1ffe0a8b48ab83fd63384e9/docs/SLEY_267_ROUND2_AUDIT.md); the current round-4 re-runs and new round-5 proofs above supersede their transient-memory/legacy-policy descriptions.

The complete final verification has 12 successful local check groups, including all seven required CI commands; default and pure-Rust public API runs each pass 38 tests. The task-local ignored Cargo target now uses the fresh /tmp verification directory.
