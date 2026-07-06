# sley-bench baselines

CSV snapshots from `cargo bench -p sley-bench` for regression tracking (W70).

## Generate

```bash
cargo bench -p sley-bench --bench rev_parse 2>&1 | tee /tmp/sley-bench-rev_parse.txt
```

Record the `odb_resolve_prefix/1000` and `odb_resolve_prefix/100000` medians in
`rev_parse.csv` after each intentional perf change.

## Suites

| Bench | File | Notes |
|-------|------|-------|
| `rev_parse` | `rev_parse.csv` | `resolve_prefix` 1k/100k (W23a acceptance) |
| `pack_install` | `pack_install.csv` | Pack install throughput |
| `cat_file` | `cat_file.csv` | Object read hot path |
| `worktree_ops` | `worktree_ops.csv` | Index/worktree operations |

Initial baselines are captured on the integration branch before W90 parity gate.