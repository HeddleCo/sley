# Full matrix re-measure 2026-07-23

- Candidate: `5029c71f` (all open-item fix waves)
- Exact: **609/891** (prior full matrix 580 **+29**; documented floor 557 **+52**)
- Sley raw: PASS 591 / FAIL 290 / SKIP 10
- Assertions: **30742/32358 (95.01%)** (prior ~94.7% / 93%)
- Oracle: PASS 881 / SKIP 9 / TIMEOUT 1 (`t4053-diff-no-index`)
- Hard raw PASS→FAIL vs 2026-07-10: 15 (3 CODE-ish, 12 harness/EXACT noise)

## CODE-ish remaining hard regressions
- `t4210-log-i18n.sh`: 21 → 14 (notok=3, vector=INCOMPARABLE)
- `t4255-am-submodule.sh`: 33 → 29 (notok=4, vector=INCOMPARABLE)
- `t1305-config-include.sh`: 37 → 35 (notok=2, vector=INCOMPARABLE)

## Harness/EXACT noise (not real code losses)
- `t0450-txt-doc-vs-help.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 794→881)
- `t1092-sparse-checkout-compatibility.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 111→110)
- `t1410-reflog.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 41→41)
- `t1517-outside-repo.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 84→332)
- `t3011-common-prefixes-and-directory-traversal.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 21→21)
- `t3910-mac-os-precompose.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 29→29)
- `t4014-format-patch.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 226→226)
- `t4058-diff-duplicates.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 16→16)
- `t4072-diff-max-depth.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 76→76)
- `t5610-clone-detached.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 13→13)
- `t6437-submodule-merge.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 22→22)
- `t7528-signed-commit-ssh.sh`: raw FAIL but cell_vector=EXACT correctness=PASS (ok 29→27)

