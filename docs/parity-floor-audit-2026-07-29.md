# Parity floor audit — 2026-07-29

This audit compares the first available hosted-Linux baseline at `0b6a2bc0`
([run 29510416630](https://github.com/HeddleCo/sley/actions/runs/29510416630))
with current main `ce828284`
([run 30472985705](https://github.com/HeddleCo/sley/actions/runs/30472985705)).
Both runs completed all 891 curated Git 2.55 scripts. The complete floor table
had instead been filled from the `w90-34be785b` development sweep on macOS, so
the audit keeps its macOS values and records separate hosted-Linux ceilings.

Current main measured 625 passed / 257 failed / 9 skipped scripts and 30,965
OK / 1,430 not-OK assertions. The issue's earlier run was 626 / 256 / 9:
only `t6416-recursive-corner-cases.sh` moved, transiently from 37 to 35 OK.
An immediate focused rerun returned 37/40 with the same three upstream TODO
cells, so that difference is hosted-run noise rather than a bankable regression.

The categories are:

- **Environment-conditional**: the upstream test plan changes on Linux.
- **Genuine regression**: a hosted-Linux PASS cell became FAIL. Its higher
  floor is preserved behind an issue-linked, measured-minimum waiver, not
  lowered. A further loss still fails the gate.
- **Never achievable**: the w90/macOS value was already below floor in the
  first comparable hosted-Linux run and remained identical on current main.

| Script | Floor → current | Classification | Evidence and disposition |
|---|---:|---|---|
| `t1092-sparse-checkout-compatibility.sh` | 111 → 109 | Genuine regression (plus a Linux plan correction) | Linux has two upstream TODO cells, making 110 the clean ceiling, but cell 71 (`ls-files`) independently changed PASS→FAIL. Linux floor 110; preserve the remaining gap under [#204](https://github.com/HeddleCo/sley/issues/204). |
| `t1304-default-acl.sh` | 4 → 2 | Never achievable | The ACL/umask pair was already 2 on the first hosted-Linux run and is still 2. Linux floor 2. |
| `t1501-work-tree.sh` | 39 → 38 | Genuine regression | Cell 26 (`git diff respects work tree under .git dir`) changed PASS→FAIL while three older discovery cells recovered. #200 does not repair it; preserve floor 39 under [#208](https://github.com/HeddleCo/sley/issues/208). |
| `t3417-rebase-whitespace-fix.sh` | 1 → 0 | Never achievable | The macOS-recorded single PASS was 0 on both hosted-Linux runs. Linux floor 0. |
| `t3437-rebase-fixup-options.sh` | 10 → 9 | Genuine regression | Cell 6 (`conflicting fixup -C after fixup with custom comment string`) changed PASS→FAIL. Preserve floor 10 under [#205](https://github.com/HeddleCo/sley/issues/205). |
| `t3910-mac-os-precompose.sh` | 29 → 0 | Environment-conditional | Git and Sley both emit a skip-all plan on Linux because the filesystem does not corrupt UTF-8. Linux floor 0; macOS floor remains 29. |
| `t4103-apply-binary.sh` | 24 → 20 | Genuine regression | Four copy cells changed PASS→FAIL. Preserve floor 24 under [#203](https://github.com/HeddleCo/sley/issues/203). |
| `t4112-apply-renames.sh` | 2 → 0 | Genuine regression | Both rename/copy cells changed PASS→FAIL. Preserve floor 2 under [#203](https://github.com/HeddleCo/sley/issues/203). |
| `t4114-apply-typechange.sh` | 12 → 11 | Genuine regression | Reverse symlink-to-file typechange cell 8 changed PASS→FAIL. Preserve floor 12 under [#203](https://github.com/HeddleCo/sley/issues/203). |
| `t4126-apply-empty.sh` | 4 → 3 | Never achievable | Stable at 3 on both hosted-Linux runs; the w90 value was not reproduced. Linux floor 3. |
| `t4132-apply-removal.sh` | 11 → 8 | Never achievable | Stable at 8 on both hosted-Linux runs; the w90 value was not reproduced. Linux floor 8. |
| `t4135-apply-weird-filenames.sh` | 19 → 17 | Never achievable | Stable at 17 on both hosted-Linux runs; the w90 value was not reproduced. Linux floor 17. |
| `t4255-am-submodule.sh` | 33 → 29 | Environment-conditional | Git and Sley both mark four submodule replacement cells TODO on Linux. Linux floor 29. |
| `t5000-tar-tree.sh` | 87 → 86 | Never achievable | Stable at 86 on both hosted-Linux runs, including the platform/tool-dependent large-size and far-future archive cases. Linux floor 86. |
| `t5003-archive-zip.sh` | 81 → 80 | Genuine regression | Big-file delta-chain cell 78 changed PASS→FAIL. Preserve floor 81 under [#206](https://github.com/HeddleCo/sley/issues/206). |
| `t5534-push-signed.sh` | 9 → 4 | Never achievable | Stable at 4 on both hosted-Linux runs; the macOS signing-toolchain value was not reproduced. Linux floor 4. |
| `t5573-pull-verify-signatures.sh` | 16 → 1 | Never achievable | Stable at 1 on both hosted-Linux runs; the macOS signing-toolchain value was not reproduced. Linux floor 1. |
| `t7004-tag.sh` | 231 → 230 | Never achievable | The double-signature cell is stable-failing on both hosted-Linux runs. Linux floor 230. |
| `t7030-verify-tag.sh` | 16 → 10 | Never achievable | The six X.509 verification cells are stable-failing on both hosted-Linux runs. Linux floor 10. |
| `t7519-status-fsmonitor.sh` | 19 → 18 | Never achievable | Stable at 18 on both hosted-Linux runs; the macOS fsmonitor value was not reproduced. Linux floor 18. |
| `t7528-signed-commit-ssh.sh` | 29 → 27 | Environment-conditional | The Linux plan has three upstream TODO cells and a stable raw OK count of 27. Linux floor 27. |
| `t7612-merge-verify-signatures.sh` | 16 → 3 | Never achievable | Stable at 3 on both hosted-Linux runs; the macOS signing-toolchain value was not reproduced. Linux floor 3. |
| `t7900-maintenance.sh` | 72 → 71 | Never achievable | Stable at 71 on both hosted-Linux runs; the daemon/lock cell from w90 was not reproduced. Linux floor 71. |
| `t9301-fast-import-notes.sh` | 14 → 11 | Never achievable | Stable at 11 on both hosted-Linux runs; the w90 value was not reproduced. Linux floor 11. |
| `t9305-fast-import-signatures.sh` | 21 → 7 | Never achievable | Stable at 7 on both hosted-Linux runs across the hosted signing toolchain. Linux floor 7. |
| `t9306-fast-import-signed-tags.sh` | 19 → 5 | Never achievable | Stable at 5 on both hosted-Linux runs across the hosted signing toolchain. Linux floor 5. |

## Apply-family remeasurement

The six requested scripts measure 59/80 assertions on current main:

| Script | `0b6a2bc0` | First bad `04e34243` | Current `ce828284` |
|---|---:|---:|---:|
| `t4103-apply-binary.sh` | 24 | 20 | 20 |
| `t4112-apply-renames.sh` | 2 | 0 | 0 |
| `t4114-apply-typechange.sh` | 12 | 11 | 11 |
| `t4126-apply-empty.sh` | 3 | 3 | 3 |
| `t4132-apply-removal.sh` | 8 | 8 | 8 |
| `t4135-apply-weird-filenames.sh` | 17 | 17 | 17 |

A first-bad bisect identifies `04e34243` (#170), where the new per-patch
`result_overlay` made later patches in the same input observe earlier
postimages. A direct boundary run at #191's parent `60f88abf` and at #191
itself (`82f90e9e`) produces the same six counts shown for current main,
59/80 in aggregate. #191 therefore does not move any script in either
direction. Its symlink-deposit security tests remain valid; the parity loss
is tracked separately in #203.

## #200 coordination

Current main and #200's semantic head `775cdb62` were measured as separate
complete 891-script runs. [Run 30474663771](https://github.com/HeddleCo/sley/actions/runs/30474663771)
keeps `t1501` cell 26 failing at 38/39, so the discovery fix does not resolve
that regression.

#200's run initially differed from the current-main run at four other cells:

| Script / cell | Current main | #200 | Independent PR repeat | Disposition |
|---|---:|---:|---:|---|
| `t0300-credentials.sh` #48 | PASS | FAIL | FAIL | Run-conditional; not attributable to #200 |
| `t5003-archive-zip.sh` #82 | PASS | FAIL | PASS | #200-specific loss |
| `t5702-protocol-v2.sh` #59 | PASS | FAIL | PASS | #200-specific loss |
| `t7510-signed-commit.sh` #28 | PASS | FAIL | PASS | #200-specific loss |

The aggregate moves from 625 passed / 257 failed scripts and 30,965 OK /
1,430 not-OK assertions to 623 passed / 259 failed and 30,961 OK / 1,434
not-OK in that direct pair. However, the later #201 PR run without #200 also
loses `t0300` #48, while recovering the independently flaky `t6416` cells
noted above. Comparing #200 with that repeat isolates three #200-specific
PASS→FAIL cells: `t5003` #82, `t5702` #59, and `t7510` #28. The oracle remains
clean at 883 passed / 0 failed / 8 skipped. Because #200 is not merged and
introduces those losses, these floors remain banked against current main; the
#200 results are reported separately on
[issue #200](https://github.com/HeddleCo/sley/issues/200#issuecomment-5121361757).

## Final gate verification

PR-head [run 30476383650](https://github.com/HeddleCo/sley/actions/runs/30476383650)
completed all 891 scripts at commit `25b8fd01`: 625 passed / 257 failed / 9
skipped and 30,966 OK / 1,429 not-OK assertions. The floor gate passed with the
seven genuine losses bounded by their open issues; no floor was lowered for
them. Relative to the earlier current-main run, `t0300` cell 48 changed
PASS→FAIL while the two flaky `t6416` cells recovered, for a net gain of one
OK assertion.
