# SLEY-1 round 5 — historical BUDGET / BLOCKED stop

Superseded by the owner’s 2026-11-18 ruling raising the stop to +650.
Work resumed; this document preserves the earlier stop and its evidence.

Stopped before pushing. PR #268 remains draft at remote head `170c8053`.
The plain merge of `origin/main` (`6ec35770`) completed without conflicts in
local commit `7723e934`. Round-5 source edits remain local and uncommitted.

The cumulative working production diff is **+533**, exceeding the owner's
**+525 stop by 8 lines**. This is a work-in-progress count, not a proven minimum.
The remaining requirements were not removed to fit the cap.

| Production file | Added | Deleted | Net |
|---|---:|---:|---:|
| `crates/sley-core/src/lib.rs` | 8 | 0 | +8 |
| `crates/sley-pack/src/index.rs` | 5 | 0 | +5 |
| `crates/sley-pack/src/lib.rs` | 2 | 0 | +2 |
| `crates/sley-pack/src/parallel_index.rs` | 78 | 756 | -678 |
| `crates/sley-pack/src/read.rs` | 17 | 0 | +17 |
| `crates/sley-pack/src/resolved.rs` | 1179 | 0 | +1179 |
| **Total against origin/main (`6ec35770`)** | **1289** | **756** | **+533** |

Gross production churn: **2045 lines**. Round 4 was +358; the local round-5
changes add +175 (8 core, 167 resolver). Main's dependency/lockfile changes are
inherited from the requested merge and are excluded from the lane diff.
Tests and evidence documents are excluded from production counts.

The local draft restores speculative parallel discovery only when every resource
limit is unlimited, skips the work mutex/operation counter/workspace gate under
that same policy, adds distinct ResourceLimitKind values for object count,
object bytes, decoded work and workspace fit, makes ResolveError non-exhaustive,
moves caller cancellation to the public visitor boundary, restores Inflate and
DeltaResolution spans, changes root metadata to u32, and removes duplicate output
metadata. These changes have not completed the requested validation.

The existing 26-test public API target ran after the draft changes:

```text
running 26 tests
...
test declared_count_preallocation_is_capped_before_parsing ... FAILED
...
failures:

---- declared_count_preallocation_is_capped_before_parsing stdout ----

running 1 test
declared count: virtual peak growth=165326848, bound=8388608
test declared_count_preallocation_is_capped_before_parsing ... FAILED

thread 'declared_count_preallocation_is_capped_before_parsing' (373) panicked at crates/sley-pack/tests/resolved.rs:1612:9:
reservation followed unparsed header count

failures:
    declared_count_preallocation_is_capped_before_parsing

test result: FAILED. 25 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 20.56s
```

This is an existing regression test failure on the new unlimited discovery path,
not a requested guard-reverted RED proof. In particular, trusted discovery needs
its preallocation and malformed-first-entry behavior corrected before delivery.
The public real-pack byte-identity/exactly-once/instruction reproduction test
passed separately before the final local metadata/progress adjustments, and also
passed in the final 26-test run.

Outstanding: correct the failing trusted-discovery test, finish and test complete
metadata accounting, add and run new typed-error/cancellation/metadata tests,
prove unlimited-policy isolation through all option routes, rerun round-4 REDs,
run the interleaved >=400k-object timing comparison, and run the seven local CI
commands plus all hosted checks. No timing, new RED, metadata bound, or full-CI
success is claimed. The new accounting counter is still a draft; REF-map and
other auxiliary metadata need review before it can be described as complete.

No round-5 changes or merge commit were pushed. No PR was merged, approved or
closed. No versions were bumped and nothing was published. The workspace version
remains 0.13.0. The exact test output is in `/tmp/sley267-r5-current-green.log`.
