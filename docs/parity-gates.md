# Upstream parity gates

Sley treats incomplete Git compatibility as expected and a loss of already
working behavior as a regression. CI therefore gates per-script passing-cell
floors instead of requiring every upstream Git test to pass.

## Pull requests

`upstream-parity.yml` runs on every pull request. To keep the normal cost well
below the 891-script weekly sweep, PRs run the scripts listed in
`.github/workflows/parity-pr-scripts.txt`. The list covers repository setup,
objects, refs, index/worktree boundaries, and the release's repaired upstream
regressions. A missing result, an aborted runner, a zero-script result, or an
`ok` count below a recorded Linux floor fails the PR check.

This is intentionally a sampling gate. It catches regressions in the selected
high-risk surfaces but cannot prove that an unrelated upstream script held its
floor. The scheduled and manually dispatched runs retain the complete curated
891-script sweep for that wider coverage.

The floor checker accepts an optional required-script file for the PR lane. Its
failure path is exercised by the workflow-contract tests using a synthetic
below-floor result and a header-only zero-script result.

## Platform matrix

`upstream-parity-matrix.yml` lets the Sley runner return nonzero for expected
incomplete parity, then applies the same per-file floor gate. Oracle runner
failures remain fatal, and the floor checker requires every tracked script, so
missing or empty output cannot become green. The optional manual `enforce`
input still runs the stricter 100% oracle-applicable correctness check.

The checked-in catalog is the measured macOS baseline. Linux applies only its
measured environment-specific overrides and waivers. Platform corrections must
be measured in that lane; they must never be copied from another platform just
to turn a cell green. The checker also refuses a full floor catalog smaller
than the predecessor gate's 891-script surface, so changing acceptance from
100% parity to regression floors cannot silently narrow matrix coverage.

Windows remains in the matrix and now builds past the Unix-only `OwnedFd`
service boundary, but no successful Windows parity summary is retained in the
Actions history. Until that lane produces a summary and its floors are reviewed,
the checker fails with an explicit calibration error instead of silently using
the macOS catalog. The uploaded matrix artifacts are the input to that one-time
calibration.
