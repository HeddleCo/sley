# sley-bench baselines

CSV snapshots from `cargo bench -p sley-bench` for regression tracking (W70).

## Generate

```bash
cargo bench -p sley-bench --bench rev_parse 2>&1 | tee /tmp/sley-bench-rev_parse.txt
```

Record the `odb_resolve_prefix/1000` (`rev_parse_oid_resolve_1k`) and
`odb_resolve_prefix/100000` (`rev_parse_oid_resolve_100k`) medians in
`rev_parse.csv` after each intentional perf change.

The 100k fixture is built on first bench run via
[`LARGE_FIXTURE_OBJECT_COUNT`](../../src/lib.rs); allow several minutes for the
initial pack write.

## Suites

| Bench | File | Notes |
|-------|------|-------|
| `rev_parse` | `rev_parse.csv` | `resolve_prefix` 1k/100k (W23a acceptance) |
| `pack_install` | `pack_install.csv` | Pack install throughput |
| `repack_measure` | stderr counters | Pack-writer and prepared-vs-legacy ODB repack material counters |
| `cat_file` | `cat_file.csv` | Object read hot path |
| `worktree_ops` | `worktree_ops.csv` | Index/worktree operations |

Initial baselines are captured on the integration branch before W90 parity gate.

`repack_measure` runs two deterministic material-efficiency contracts before
Criterion sampling. Capture their counters with:

```bash
cargo bench -p sley-bench --bench repack_measure -- --quick 2>&1 \
  | tee /tmp/sley-bench-repack_measure.txt
```

The `pack_writer` line is a pack-layer microcontract. Its versioned deterministic
baseline is 510,291 charged working-set bytes, 5,898 pack bytes, and 254 deltas;
the gates retain at least 95% of each baseline's quality. “Charged” is precise:
it excludes the output sink, index, zlib buffers, allocator slack, and fixture.

The `repository_repack` line exercises the public ODB APIs on a 4,098-object
commit/tree/blob repository, above the production 4,096-object streaming
threshold. It compares prepared output with the legacy in-memory result from
the same fixture. The versioned prepared baseline is 8,196 total body reads,
192,461 staged pack bytes, 115,816 index bytes, 4,084 deltas, and maximum delta
depth 46. The contract requires byte-identical checksum/index output, at least
95% of both this recorded baseline and the same-run legacy pack-size and
delta-density quality, no pack payload resident in the prepared result, no
increase in body reads, and delta depth at or below policy. The completed
staged pack is hard-linked under a `.pack` name and read through `sley_mmap`, so
verification does not reintroduce a pack-sized heap buffer. On the baseline
run, the legacy result retained a 319,488-byte pack allocation while the
prepared result retained zero pack-output bytes.

Criterion reports `legacy_in_memory` and `prepared_file_backed` separately.
Pack/index verification and the 95% contract run before sampling and therefore
do not contaminate either timed body.
