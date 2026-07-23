# Matrix re-run 2026-07-22 (macOS, git 2.55.0, SHA-1)

- Candidate: `d65a513c` (wave1+wave2)
- Oracle: 882 PASS / 9 SKIP / 0 FAIL
- Sley raw: 563 PASS / 318 FAIL / 10 SKIP
- Exact cell vectors: **580/891** (prior floor 557, **+23**)
- Assertions: 30630/32358 (94.66%)
- Floor gate: FAILED (52 drops)

## Floor drops (ok below recorded floor)
- `FAIL: t1092-sparse-checkout-compatibility.sh: ok=110 dropped below floor=111 (result=FAIL)`
- `FAIL: t3701-add-interactive.sh: ok=116 dropped below floor=130 (result=FAIL)`
- `FAIL: t3910-mac-os-precompose.sh: ok=17 dropped below floor=29 (result=FAIL)`
- `FAIL: t4014-format-patch.sh: ok=224 dropped below floor=226 (result=FAIL)`
- `FAIL: t7105-reset-patch.sh: ok=4 dropped below floor=5 (result=FAIL)`
- `FAIL: t7408-submodule-reference.sh: ok=11 dropped below floor=14 (result=FAIL)`
- `FAIL: t7506-status-submodule.sh: ok=37 dropped below floor=38 (result=FAIL)`
- `FAIL: t0090-cache-tree.sh: ok=22 dropped below floor=23 (result=FAIL)`
- `FAIL: t1020-subdirectory.sh: ok=11 dropped below floor=15 (result=FAIL)`
- `FAIL: t3903-stash.sh: ok=141 dropped below floor=143 (result=FAIL)`
- `FAIL: t6418-merge-text-auto.sh: ok=4 dropped below floor=5 (result=FAIL)`
- `FAIL: t7900-maintenance.sh: ok=71 dropped below floor=72 (result=FAIL)`
- `FAIL: t0002-gitfile.sh: ok=12 dropped below floor=14 (result=FAIL)`
- `FAIL: t3417-rebase-whitespace-fix.sh: ok=0 dropped below floor=1 (result=FAIL)`
- `FAIL: t5319-multi-pack-index.sh: ok=97 dropped below floor=98 (result=FAIL)`
- `FAIL: t7400-submodule-basic.sh: ok=116 dropped below floor=117 (result=FAIL)`
- `FAIL: t0003-attributes.sh: ok=54 dropped below floor=55 (result=FAIL)`
- `FAIL: t0204-gettext-reencode-sanity.sh: ok=6 dropped below floor=8 (result=FAIL)`
- `FAIL: t1800-hook.sh: ok=91 dropped below floor=92 (result=FAIL)`
- `FAIL: t4210-log-i18n.sh: ok=17 dropped below floor=21 (result=PASS)`
- `FAIL: t5813-proto-disable-ssh.sh: ok=69 dropped below floor=81 (result=FAIL)`
- `FAIL: t6120-describe.sh: ok=130 dropped below floor=131 (result=FAIL)`
- `FAIL: t7517-per-repo-email.sh: ok=14 dropped below floor=16 (result=FAIL)`
- `FAIL: t7528-signed-commit-ssh.sh: ok=27 dropped below floor=29 (result=FAIL)`
- `FAIL: t0210-trace2-normal.sh: ok=13 dropped below floor=14 (result=FAIL)`
- `FAIL: t0410-partial-clone.sh: ok=34 dropped below floor=38 (result=FAIL)`
- `FAIL: t4132-apply-removal.sh: ok=8 dropped below floor=11 (result=FAIL)`
- `FAIL: t4255-am-submodule.sh: ok=29 dropped below floor=33 (result=FAIL)`
- `FAIL: t5521-pull-options.sh: ok=16 dropped below floor=22 (result=FAIL)`
- `FAIL: t7813-grep-icase-iso.sh: ok=1 dropped below floor=2 (result=FAIL)`
- `FAIL: t0211-trace2-perf.sh: ok=14 dropped below floor=17 (result=FAIL)`
- `FAIL: t2016-checkout-patch.sh: ok=17 dropped below floor=19 (result=FAIL)`
- `FAIL: t2071-restore-patch.sh: ok=10 dropped below floor=15 (result=FAIL)`
- `FAIL: t4052-stat-output.sh: ok=89 dropped below floor=91 (result=FAIL)`
- `FAIL: t6030-bisect-porcelain.sh: ok=95 dropped below floor=97 (result=FAIL)`
- `FAIL: t7423-submodule-symlinks.sh: ok=5 dropped below floor=6 (result=FAIL)`
- `FAIL: t7519-status-fsmonitor.sh: ok=18 dropped below floor=19 (result=FAIL)`
- `FAIL: t1423-ref-backend.sh: ok=28 dropped below floor=36 (result=FAIL)`
- `FAIL: t3437-rebase-fixup-options.sh: ok=9 dropped below floor=10 (result=FAIL)`
- `FAIL: t3502-cherry-pick-merge.sh: ok=10 dropped below floor=12 (result=FAIL)`
- `FAIL: t4126-apply-empty.sh: ok=3 dropped below floor=4 (result=FAIL)`
- `FAIL: t5616-partial-clone.sh: ok=33 dropped below floor=37 (result=FAIL)`
- `FAIL: t5900-repo-selection.sh: ok=0 dropped below floor=8 (result=FAIL)`
- `FAIL: t6301-for-each-ref-errors.sh: ok=4 dropped below floor=6 (result=FAIL)`
- `FAIL: t1305-config-include.sh: ok=35 dropped below floor=37 (result=FAIL)`
- `FAIL: t1430-bad-ref-name.sh: ok=36 dropped below floor=40 (result=FAIL)`
- `FAIL: t1501-work-tree.sh: ok=36 dropped below floor=39 (result=FAIL)`
- `FAIL: t2105-update-index-gitfile.sh: ok=0 dropped below floor=4 (result=FAIL)`
- `FAIL: t3430-rebase-merges.sh: ok=33 dropped below floor=34 (result=FAIL)`
- `FAIL: t3700-add.sh: ok=56 dropped below floor=57 (result=FAIL)`
- `FAIL: t4135-apply-weird-filenames.sh: ok=17 dropped below floor=19 (result=FAIL)`
- `FAIL: t6133-pathspec-rev-dwim.sh: ok=4 dropped below floor=6 (result=FAIL)`

## ok-count drops vs 2026-07-10 summary (top)
- `t3701-add-interactive.sh`: 130 → 116 (−14)
- `t3910-mac-os-precompose.sh`: 29 → 17 (−12)
- `t5813-proto-disable-ssh.sh`: 81 → 69 (−12)
- `t1423-ref-backend.sh`: 36 → 28 (−8)
- `t5900-repo-selection.sh`: 8 → 0 (−8)
- `t5521-pull-options.sh`: 22 → 16 (−6)
- `t2071-restore-patch.sh`: 15 → 10 (−5)
- `t1430-bad-ref-name.sh`: 40 → 36 (−4)
- `t2105-update-index-gitfile.sh`: 4 → 0 (−4)
- `t5616-partial-clone.sh`: 37 → 33 (−4)
- `t0410-partial-clone.sh`: 38 → 34 (−4)
- `t1020-subdirectory.sh`: 15 → 11 (−4)
- `t4210-log-i18n.sh`: 21 → 17 (−4)
- `t4255-am-submodule.sh`: 33 → 29 (−4)
- `t4132-apply-removal.sh`: 11 → 8 (−3)
- `t0211-trace2-perf.sh`: 17 → 14 (−3)
- `t7408-submodule-reference.sh`: 14 → 11 (−3)
- `t1501-work-tree.sh`: 39 → 36 (−3)
- `t7528-signed-commit-ssh.sh`: 29 → 27 (−2)
- `t6301-for-each-ref-errors.sh`: 6 → 4 (−2)
- `t1305-config-include.sh`: 37 → 35 (−2)
- `t0204-gettext-reencode-sanity.sh`: 8 → 6 (−2)
- `t4135-apply-weird-filenames.sh`: 19 → 17 (−2)
- `t7105-reset-patch.sh`: 6 → 4 (−2)
- `t2016-checkout-patch.sh`: 19 → 17 (−2)
- `t4052-stat-output.sh`: 91 → 89 (−2)
- `t6030-bisect-porcelain.sh`: 97 → 95 (−2)
- `t4014-format-patch.sh`: 226 → 224 (−2)
- `t6133-pathspec-rev-dwim.sh`: 6 → 4 (−2)
- `t7517-per-repo-email.sh`: 16 → 14 (−2)

Net ok-cells vs 2026-07-10: **+1032**

## Hard regressions (was clean PASS on 2026-07-10, now FAIL)

- `t3701-add-interactive.sh`: 130/… → 116 ok / 14 fail
- `t3910-mac-os-precompose.sh`: 29/… → 17 ok / 12 fail
- `t5813-proto-disable-ssh.sh`: 81/… → 69 ok / 12 fail
- `t1423-ref-backend.sh`: 36/… → 28 ok / 8 fail
- `t5900-repo-selection.sh`: 8/… → 0 ok / 8 fail
- `t5521-pull-options.sh`: 22/… → 16 ok / 6 fail
- `t2071-restore-patch.sh`: 15/… → 10 ok / 5 fail
- `t2105-update-index-gitfile.sh`: 4/… → 0 ok / 4 fail
- `t1020-subdirectory.sh`: 15/… → 11 ok / 4 fail
- `t4255-am-submodule.sh`: 33/… → 29 ok / 4 fail
- `t0211-trace2-perf.sh`: 17/… → 14 ok / 3 fail
- `t1501-work-tree.sh`: 39/… → 36 ok / 3 fail
- `t7528-signed-commit-ssh.sh`: 29/… → 27 ok / 2 fail
- `t6301-for-each-ref-errors.sh`: 6/… → 4 ok / 2 fail
- `t1305-config-include.sh`: 37/… → 35 ok / 2 fail
- `t0204-gettext-reencode-sanity.sh`: 8/… → 6 ok / 2 fail
- `t2016-checkout-patch.sh`: 19/… → 17 ok / 2 fail
- `t4052-stat-output.sh`: 91/… → 89 ok / 2 fail
- `t6030-bisect-porcelain.sh`: 97/… → 95 ok / 2 fail
- `t4014-format-patch.sh`: 226/… → 224 ok / 2 fail
- `t6133-pathspec-rev-dwim.sh`: 6/… → 4 ok / 2 fail
- `t7517-per-repo-email.sh`: 16/… → 14 ok / 2 fail
- `t3502-cherry-pick-merge.sh`: 12/… → 10 ok / 2 fail
- `t0002-gitfile.sh`: 14/… → 12 ok / 2 fail
- `t0003-attributes.sh`: 55/… → 54 ok / 1 fail
- `t1800-hook.sh`: 92/… → 91 ok / 1 fail
- `t5319-multi-pack-index.sh`: 98/… → 97 ok / 1 fail
- `t0210-trace2-normal.sh`: 14/… → 13 ok / 1 fail
- `t1092-sparse-checkout-compatibility.sh`: 111/… → 110 ok / 1 fail
- `t0090-cache-tree.sh`: 23/… → 22 ok / 1 fail
- `t7423-submodule-symlinks.sh`: 6/… → 5 ok / 1 fail
- `t7813-grep-icase-iso.sh`: 2/… → 1 ok / 1 fail
- `t4058-diff-duplicates.sh`: 16/… → 16 ok / 0 fail
- `t4072-diff-max-depth.sh`: 76/… → 76 ok / 0 fail
- `t5610-clone-detached.sh`: 13/… → 13 ok / 0 fail
- `t6437-submodule-merge.sh`: 22/… → 22 ok / 0 fail
- `t1410-reflog.sh`: 41/… → 41 ok / 0 fail
- `t3011-common-prefixes-and-directory-traversal.sh`: 21/… → 21 ok / 0 fail
- `t0450-txt-doc-vs-help.sh`: 794/… → 881 ok / 15 fail
- `t1517-outside-repo.sh`: 84/… → 337 ok / 31 fail

## Wave 1–2 exact confirmations

- `t2024-checkout-dwim.sh`: EXACT PASS
- `t6300-for-each-ref.sh`: EXACT PASS
- `t1461-refs-list.sh`: EXACT PASS
- `t4034-diff-words.sh`: EXACT PASS
- `t4018-diff-funcname.sh`: EXACT PASS
- `t1308-config-set.sh`: EXACT PASS
- `t1504-ceiling-dirs.sh`: EXACT PASS
- `t7005-editor.sh`: EXACT PASS
- `t1402-check-ref-format.sh`: EXACT PASS
- `t5505-remote.sh`: EXACT PASS
- `t7502-commit-porcelain.sh`: EXACT PASS
- `t5520-pull.sh`: EXACT PASS
- `t0300-credentials.sh`: EXACT PASS
- `t2003-checkout-cache-mkdir.sh`: EXACT PASS
- `t2011-checkout-invalid-head.sh`: EXACT PASS

## Residual wave near-misses still failing

- `t0001-init.sh` #28 umask vs shared deep dir (GIT_OBJECT_DIRECTORY #103 green)
- `t3700-add.sh` #8/#10 filemode=0 symlink confusion (not the refresh cluster)
- `t0014-alias.sh` #4/#8 deprecated builtin alias loops (dotted alias cells green)
- `t2200-add-update.sh` #17 unmerged paths
- `t5319-multi-pack-index.sh` #77 expire removes repacked packs (was exact 98/98 at floor 557)
