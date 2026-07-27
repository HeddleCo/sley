# Git parity wave results through 2026-07-25

PR [#168](https://github.com/HeddleCo/sley/pull/168) recorded five full
891-script matrix checkpoints while parity work was developed. The original
banked `matrix-rerun-*` directories are intentionally not retained: they held
117 regenerable CSV, log, and report files with 635,290 added lines. The
replacement branches preserve the result progression and rerun the matrix from
their actual commits.

These are historical measurements from the original commit series, not claims
about the rebased replacement tip:

| Original commit | EXACT | Change | Wave outcomes recorded by the commit history |
| --- | ---: | ---: | --- |
| `37db0abd` | 580/891 | baseline checkpoint | Near-miss work for init, add, checkout, and word-diff, followed by alias/help, discovery, config, ref-format, remote advice, commit, and pull/rebase behavior. |
| `e4951b27` | 609/891 | +29 | Restored hard regressions in interactive add, multi-pack-index expiry, repository/worktree discovery, pull/fetch, ref backends, SSH, Unicode precomposition, patch/rebase/submodule/trace/i18n behavior, then closed the remaining am, includeIf, and log-i18n cases. |
| `6e1a72a2` | 628/891 | +19 | Landed namespace/hideRefs, backfill, protocol-v2 negotiate-only, sparse partial-clone checkout, commit porcelain, upload-pack `want-ref`, repack, commit-graph, and protocol near-misses. |
| `32e5aedd` | 636/891 (71.4%) | +8 | R0 restored ten lost EXACT scripts and N1 advanced fetch, push, update-ref, and protocol-v2 behavior. This checkpoint was measured before the sparse/CRLF residual commit; its metadata records two cells re-lost relative to the ten R0 recoveries. |
| `b4f09d64` | 656/891 (73.6%) | +20 | N1.5 closed fetch/push/update-ref residuals and harvested sparse, checkout, apply, stash, log, merge, submodule, and fast-import/export near-misses. N2 closed partial-clone/promisor cases and harvested add-edit, multi-patch apply, submodule-path, and gitlink-removal near-misses. |

The original snapshots also recorded raw PASS/assertion totals, but every banked
run identified both the candidate and upstream trees as dirty. Those payloads
therefore were not reproducible evidence for a specific commit. Replacement PR
test evidence must name the clean candidate commit and report the newly measured
EXACT count rather than copying the historical `656/891` value.
