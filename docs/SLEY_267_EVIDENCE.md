# SLEY-267 review evidence

Issue design: https://github.com/HeddleCo/sley/issues/267. Base: `a71654f5`. No version or release configuration changes. Workspace version is `0.13.0`, matching the downstream pin.

The public Rust signature uses `<V, E>` with `V: Fn(...) -> Result<(), E> + Sync` and `E: Send`; `Fn` has no `V::Error` associated type. Legacy external-base and progress hooks use the same engine. Fully materialized `PackFile` reads retain their requested output through a visitor.

The shared gate bounds retained ancestor bodies. Temporary decoding additionally needs at most two bodies and one delta stream per worker; metadata is bounded by the checked header count. Peak-memory tests run in isolated Linux subprocesses and measure `VmHWM`, including allocator and zlib overhead.

## Original engine baseline

The stored real SHA-1/SHA-256 packs contain blobs, trees, commits, an annotated tag and deltas. Before replacing the engine, all three schedules matched Git and each other. The stored `.idx` files are that original-engine output.

```text
sha1: original engine matches Git at threads 1, 2, 16
sha256: original engine matches Git at threads 1, 2, 16
```

## Guard-reverted RED evidence

Each probe temporarily removed the indicated production guard, ran the named public API test, and restored the source. Every probe exited 101 with an executed test failure, not a compilation failure. Thin-pack rejection has two completeness checks; both were removed to exercise acceptance of an unresolved pack. The cancellation probe bypassed both worker and coordinator polls; initial/final cancellation checks remained.

Selected stdout/stderr is pasted below. Compiler progress, duplicate child-process reports and large debug byte arrays are omitted.

### declared-size-guard

```text
test declared_size_bomb_is_refused_before_allocation ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s
thread 'declared_size_bomb_is_refused_before_allocation' (314) panicked at crates/sley-pack/tests/resolved.rs:344:5:
thread 'declared_size_bomb_is_refused_before_allocation' (316) panicked at crates/sley-pack/tests/resolved.rs:363:5:
assertion failed: error(&p, ResolveLimits::default()).contains("max_object_bytes")
```

### delta-depth-guard

```text
test depth_51_is_refused_before_visiting_the_deep_object ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.01s
thread '<unnamed>' (1559) panicked at crates/sley-pack/tests/resolved.rs:432:13:
assertion failed: o.depth <= 50
thread 'depth_51_is_refused_before_visiting_the_deep_object' (1558) panicked at crates/sley-pack/tests/resolved.rs:438:5:
assertion failed: pack_message(e).contains("observed depth 51")
```

### delta-instructions

```text
test real_packs_match_original_indexes_and_visit_once_with_reproducible_instructions ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.03s
thread 'real_packs_match_original_indexes_and_visit_once_with_reproducible_instructions' (3750) panicked at crates/sley-pack/tests/resolved.rs:130:21:
index out of bounds: the len is 0 but the index is 0
```

### last-dependent-release

```text
last-dependent release: peak=58011648 bytes
test last_dependent_releases_base_before_visiting_child ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 1.65s
thread 'last_dependent_releases_base_before_visiting_child' (3129) panicked at crates/sley-pack/tests/resolved.rs:344:5:
thread 'last_dependent_releases_base_before_visiting_child' (3131) panicked at crates/sley-pack/tests/resolved.rs:621:5:
completed ancestors still retained: 58011648
```

### object-count-guard

```text
test delta_result_bomb_and_running_budgets_are_checked_before_visits ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s
thread 'delta_result_bomb_and_running_budgets_are_checked_before_visits' (936) panicked at crates/sley-pack/tests/resolved.rs:416:10:
```

### result-size-guard

```text
test delta_result_bomb_and_running_budgets_are_checked_before_visits ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s
thread 'delta_result_bomb_and_running_budgets_are_checked_before_visits' (626) panicked at crates/sley-pack/tests/resolved.rs:386:5:
assertion failed: error(&p, ResolveLimits::default()).contains("max_object_bytes")
```

### shared-live-byte-gate

```text
live bases: baseline=6033408, peak=358699008, cap=8388608 bytes (16 workers)
test live_base_bound_is_shared_and_replay_stays_within_peak_memory ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 4.56s
thread 'live_base_bound_is_shared_and_replay_stays_within_peak_memory' (2801) panicked at crates/sley-pack/tests/resolved.rs:344:5:
thread 'live_base_bound_is_shared_and_replay_stays_within_peak_memory' (2803) panicked at crates/sley-pack/tests/resolved.rs:581:5:
retained bases exceeded shared gate: peak=358699008
```

### thin-completion-guards

```text
test thin_pack_is_an_unresolved_base_error ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s
thread 'thin_pack_is_an_unresolved_base_error' (317) panicked at crates/sley-pack/tests/resolved.rs:118:10:
```

### total-decoded-guard

```text
test delta_result_bomb_and_running_budgets_are_checked_before_visits ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s
thread 'delta_result_bomb_and_running_budgets_are_checked_before_visits' (1247) panicked at crates/sley-pack/tests/resolved.rs:416:10:
```

### truncated-discovery-guard

```text
test truncation_and_trailing_bytes_are_errors ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s
thread 'truncation_and_trailing_bytes_are_errors' (2179) panicked at crates/sley-pack/tests/resolved.rs:489:5:
assertion `left == right` failed: discovery must complete before visits
  left: 2
 right: 0
```

### visitor-exactly-once

```text
test forward_ref_chain_and_duplicate_oids_visit_each_entry ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s
thread 'forward_ref_chain_and_duplicate_oids_visit_each_entry' (3438) panicked at crates/sley-pack/tests/resolved.rs:316:5:
assertion `left == right` failed
  left: 0
 right: 8
```

### worker-and-coordinator-cancel-polls

```text
test cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.01s
thread 'cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error' (2490) panicked at crates/sley-pack/tests/resolved.rs:518:5:
assertion `left == right` failed
  left: 48
 right: 1
```

## Restored GREEN evidence

All seven requested checks and the additional HTTP embedding CI check exited 0. Documentation ran with warnings denied. The workspace test output contains 224 completed binary/doc-test groups: **3,796 passed, 0 failed, 2 ignored**.

| Verification | Exit |
|---|---:|
| `cargo build --locked --workspace --tests` | 0 |
| `cargo fmt --all --check` | 0 |
| `python3 .github/workflows/scripts/clippy-panic-gate.py` | 0 |
| `cargo clippy --locked --workspace --all-targets --all-features --no-deps -- -D warnings` | 0 |
| `cargo test --locked --workspace` | 0 |
| `cargo doc --locked --workspace --no-deps --lib` | 0 |
| `bash scripts/check-publish-pipeline.sh` | 0 |
| `bash scripts/check-http-embedding.sh` | 0 |

Final public API test output (default zlib-rs backend):

```text
test truncation_and_trailing_bytes_are_errors ... ok
test thin_pack_is_an_unresolved_base_error ... ok
test delta_result_bomb_and_running_budgets_are_checked_before_visits ... ok
bomb: peak=4509696 bytes, growth=151552 bytes
test declared_size_bomb_is_refused_before_allocation ... ok
test declared_size_bomb_is_refused_before_allocation ... ok
test forward_ref_chain_and_duplicate_oids_visit_each_entry ... ok
test depth_51_is_refused_before_visiting_the_deep_object ... ok
test cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error ... ok
test real_packs_match_original_indexes_and_visit_once_with_reproducible_instructions ... ok
last-dependent release: peak=7704576 bytes
test last_dependent_releases_base_before_visiting_child ... ok
test last_dependent_releases_base_before_visiting_child ... ok
live bases: baseline=6045696, peak=57704448, cap=8388608 bytes (16 workers)
test live_base_bound_is_shared_and_replay_stays_within_peak_memory ... ok
test live_base_bound_is_shared_and_replay_stays_within_peak_memory ... ok
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.23s
```

Pure-Rust backend:

```text
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.86s
```

Selected final gate output:

```text
Finished `dev` profile [unoptimized + debuginfo] target(s) in 40.72s
Finished `dev` profile [unoptimized + debuginfo] target(s) in 36.13s
Finished `dev` profile [unoptimized + debuginfo] target(s) in 16.56s
ok: explicit CRATES=( ... ) list present
ok: workflow reads secrets.CRATES_IO_API_KEY
ok: CARGO_REGISTRY_TOKEN env var declared
ok: validator self-test: correct passes; drop / bogus / mis-order / dup all fail
ok: CRATES matches the publishable set exactly (31 crates)
ok: CRATES is topologically ordered (149 in-set dependency edges)
publish-pipeline check passed
```

The parent contract also requested an export lint, which does not exist in this repository. No replacement gate or dependency was added:

```text
error: no such command: `xtask`
```


## Net production budget

Relative to `a71654f5`; tests, fixtures and review evidence are excluded from production counts.

| Production file | Added | Removed | Net |
|---|---:|---:|---:|
| `crates/sley-pack/src/lib.rs` | 2 | 0 | +2 |
| `crates/sley-pack/src/parallel_index.rs` | 59 | 754 | -695 |
| `crates/sley-pack/src/resolved.rs` | 742 | 0 | +742 |
| **Total** | **803** | **754** | **+49** |

**49 / 420 target; below the +525 stop.** The old speculative scanner and dependency-level indexer were deleted; repository-wide searches found no remaining references to those private helpers. Public entry points and signatures remain available. New tests and fixture provenance are under `crates/sley-pack/tests/`; this document contains review evidence only.
