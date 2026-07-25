# Unmerged parity lanes audit (2026-07-25)

Attempted cherry-pick of leftover `origin/parity/r6–r10`, `origin/wave/*`, etc.
onto `parity/exact-628-r0-n1-wave` / `9827c573`.

## Result: cherry-pick not viable; work already present

All 13 single-commit r6–r10 lanes **conflicted** with current tree (code moved).
Baseline smoke of their claimed scripts on **current tip** shows targets already
met or exceeded:

| Script | Claimed floor | Current | Status |
|--------|-------------:|--------:|--------|
| t1800-hook | 92 | **92/92** | already green |
| t4211-line-log | 95 | **95/95** | already green |
| t1300-config | 516 | **516/516** | already green |
| t1400-update-ref | 315 | **315/315** | already green |
| t3700-add | 57 | **58/58** | already green |
| t2400-worktree-add | 232 | **232/232** | already green |
| t3701-add-interactive | 130 | **130/130** | already green |
| t3903-stash | 143 | **143/145** (2 TODO) | already green |
| t4052-stat-output | 91 | **91/91** | already green |
| t4045-diff-relative | 38 | **38/39** (TODO) | already green |
| t1423-ref-backend | 36 | **36/36** | already green |
| t3905-stash-include-untracked | 34 | **34/34** | already green |
| t9350-fast-export | 60 | **72/73** (TODO) | already green |

**Conclusion:** leftover remote branches are **stale SHAs**, not missing features.
Later evolution already absorbed those gains under different commits.

## Wave/* residual (still incomplete on tip)

| Script | Current | Gap theme |
|--------|--------:|-----------|
| t3602-rm-sparse-checkout | 8/13 | sparse rm gate |
| t3705-add-sparse-checkout | 18/20 | sparse add edge |
| t0020-crlf | 35/36 | existing .gitattributes checkout |
| t5510-fetch | 219/230 | atomic, hideRefs, case-fold FS |

These need **forward ports / new fixes**, not raw cherry-picks.

## 96% recollection

No unmerged branch banks a full-suite exact >70.5%. Closest high number on
this line is **assertion pass rate 95.59%** (banked full matrix).
