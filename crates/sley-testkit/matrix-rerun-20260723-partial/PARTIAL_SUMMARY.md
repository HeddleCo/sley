# Partial matrix re-bank (2026-07-23)

Candidate: post-regression-fix commit (before P0 wave integrate).

## Scripts (all EXACT PASS vs oracle git 2.55.0)

| script | ok | vector |
|--------|---:|--------|
| t3701-add-interactive.sh | 130 | EXACT |
| t5319-multi-pack-index.sh | 98 | EXACT |
| t3700-add.sh | 58 | EXACT |
| t5900-repo-selection.sh | 8 | EXACT |
| t2105-update-index-gitfile.sh | 4 | EXACT |
| t6133-pathspec-rev-dwim.sh | 6 | EXACT |
| t6301-for-each-ref-errors.sh | 6 | EXACT |
| t3430-rebase-merges.sh | 34 | EXACT |

**8/8 scripts EXACT**, 341 TAP pass cells, 0 fail.
