# Hard-regression triage — matrix re-run 2026-07-22

- **Candidate:** `d65a513c` (wave-1 `d11b99f3` + wave-2 `d65a513c`)
- **Baseline:** `crates/sley-testkit/upstream-summary-sley-20260710.csv` (raw PASS, `notok=0`)
- **Current:** `matrix-rerun-20260722/upstream-sley-summary.csv` (raw FAIL)
- **Oracle:** git 2.55.0 (`/tmp/git-prefix/bin/git`), macOS arm64, SHA-1
- **Sley binary used in matrix:** `…/2026-07-22-be8ad20f/target/release/sley`
- **Method:** recompute hard list from CSVs; extract fail titles from `upstream-sley-cells.csv` + report; cell-compare vs oracle; hermetic spot-checks for top scripts; light commit correlation (wave1/2 + recent parity commits)

## Executive summary

| Class | Count | Meaning |
|-------|------:|---------|
| **CODE** | **28** | Real behavioral regressions vs 2026-07-10 clean PASS (SLEY_FAILURE cells vs oracle PASS) |
| **ORACLE/HARNESS** | **10** | No real fail cells vs oracle; raw FAIL from `test_expect_failure` “known breakage vanished” and/or residual TODOs (exit 1). Cell vectors EXACT, correctness PASS |
| **ENV** | **2** | Locale / i18n / PCRE platform surface, not wave1/2 code |
| **UNCLEAR** | **0** | — |
| **Total hard regressions** | **40** | Matches orchestrator (~40); recomputed exactly |

**Net context:** matrix still gained **+1032 ok-cells** and **+23 exact cell vectors** vs prior floor. These 40 are the no-regression gate blockers (was clean PASS, now FAIL).

**Important harness nuance:** for several scripts sley now *passes* `test_expect_failure` cells (`raw_result=ok` + TODO directive) while oracle still fails them (`raw_result=not_ok`). Git’s harness then exits 1 with “known breakage(s) vanished”. The cell-comparison layer currently treats both as `TODO` and may report EXACT/PASS even when `raw_result` differs — floor gates still fail on raw script result / ok-count. These are **not** functional regressions; treat as ORACLE/HARNESS.

### Spot-check confirmation (hermetic, this host)

| Script | Oracle | Sley (repro) |
|--------|--------|--------------|
| `t5900-repo-selection.sh` | 8/0 PASS | **0/8 FAIL** |
| `t2105-update-index-gitfile.sh` | 4/0 PASS | **0/4 FAIL** |
| `t1020-subdirectory.sh` | 15/0 PASS | **11/4 FAIL** |
| `t6133-pathspec-rev-dwim.sh` | 6/0 PASS | **4/2 FAIL** |
| `t3910-mac-os-precompose.sh` | (matrix PASS 28+1todo) | **17/12 FAIL** |
| `t1423-ref-backend.sh` | (matrix PASS 36) | **28/8 FAIL** |
| `t0002-gitfile.sh` | (matrix PASS) | **12/2 FAIL** |
| `t6301-for-each-ref-errors.sh` | (matrix PASS) | **4/2 FAIL** |

---

## Classification table

Columns: `prev_ok` / `now_ok` from 2026-07-10 vs matrix sley summaries; `fail_cells` = SLEY_FAILURE count vs oracle (not TAP notok, which sometimes counts TODOs).

| script | prev_ok | now_ok | fail_cells | class | rationale | owner crate(s) | pri |
|--------|--------:|-------:|-----------:|-------|-----------|----------------|-----|
| t3701-add-interactive.sh | 130 | 116 | 14 | **CODE** | Patch edit/split/mode staging cluster regresses (real edit, strip empty context, mode hunks, pathological context, --no-advance multi-file). Was completed to 130 in #134; wave1 `plumbing/add.rs` + interactive path likely re-broke. | sley-cli (add -i/-p) | **P0** |
| t3910-mac-os-precompose.sh | 29 | 17 | 12 | **CODE** | Oracle full pass on same macOS FS; sley fails NFD detect/diff/stage/log/mv/checkout. Not env flake — precomposeunicode path normalization broken. (+1 TODO vanished) | sley-cli / sley-worktree / path layer | **P0** |
| t5813-proto-disable-ssh.sh | 81 | 69 | 12 | **CODE** | All failures are **enabled** `git+ssh://` clone/fetch/push (disabled cells still ok). Oracle 81/0. SSH transport or scheme alias (`git+ssh`→`ssh`) regression when protocol allowed. | sley-remote (protocol/ssh), sley-cli | **P0** |
| t1423-ref-backend.sh | 36 | 28 | 8 | **CODE** | Worktree + alt backend (reftable/files) via config/env: for-each-ref/rev-parse from linked worktree diverge. Regression of #138 route work (29→36 then back). | sley-refs / sley-cli worktree+backend URI | **P0** |
| t5900-repo-selection.sh | 8 | 0 | 8 | **CODE** | Total collapse of local path DWIM (`.git` suffix, bare vs worktree prefer, non-git dir). Spot-check 0/8. Likely adjacent to wave2 discovery/`open_env` work even if not intended. | sley / sley-cli discovery, enter_repo | **P0** |
| t5521-pull-options.sh | 22 | 16 | 6 | **CODE** | `--no-rebase`/`--rebase`/`-v`/`-q -v`/`--force` fail. Wave2 heavily rewrote `pull.rs`. | sley-cli merge_rebase/pull | **P0** |
| t2071-restore-patch.sh | 15 | 10 | 5 | **CODE** | `restore -p --source=HEAD/@/HEAD^/HEAD^...` and path limiting. Interactive patch path (w49). | sley-cli restore -p | **P1** |
| t2105-update-index-gitfile.sh | 4 | 0 | 4 | **CODE** | Absolute/relative gitfile submodules + `update-index --add` gitlink all fail (spot 0/4). | sley-cli update-index, gitfile resolve | **P1** |
| t1020-subdirectory.sh | 15 | 11 | 4 | **CODE** | No file/rev ambiguity inside `.git` / bare / symlink-fooled detection. Spot 11/4. | sley-cli setup/discovery, rev-parse DWIM | **P1** |
| t4255-am-submodule.sh | 33 | 29 | 2 | **CODE** | `am` / `am -3` replace tracked file with submodule empty-dir. (+ harness TODOs) | sley-cli am, unpack-trees/gitlink | **P1** |
| t0211-trace2-perf.sh | 17 | 14 | 3 | **CODE** | URL redaction + def_params for remote-curl / http-fetch dashed helpers. May also need helper wiring (see ENV notes). | sley-cli trace2, transport | **P1** |
| t1501-work-tree.sh | 39 | 36 | 3 | **CODE** | Auto discovery + `$GIT_DIR/common` vs `core.worktree` / `$GIT_WORK_TREE` override. Discovery/open path. | sley open_env, sley-cli discovery | **P1** |
| t6301-for-each-ref-errors.sh | 6 | 4 | 2 | **CODE** | Broken refs + NULL_SHA1 not warned/ignored like git. Wave1 touched for-each-ref helpers. | sley-cli for_each_ref / sley-refs | **P1** |
| t2016-checkout-patch.sh | 19 | 17 | 2 | **CODE** | `checkout -p HEAD/@` abort with no staged changes. Same interactive-patch family as t2071. | sley-cli checkout -p | **P1** |
| t6133-pathspec-rev-dwim.sh | 6 | 4 | 2 | **CODE** | `^{/re}` and `@{when}` with metacharacters DWIM to rev (spot confirmed). | sley-rev / pathspec DWIM | **P1** |
| t0002-gitfile.sh | 14 | 12 | 2 | **CODE** | `update-index` + `setup_git_dir` twice in subdir via gitfile. | sley-cli gitfile / setup | **P1** |
| t0090-cache-tree.sh | 23 | 22 | 1 | **CODE** | `commit --interactive` partial commit cache-tree; tied to add -i. | sley-cli commit -i / cache-tree | **P1** |
| t0210-trace2-normal.sh | 14 | 13 | 1 | **CODE** | Unsafe URL redaction default (same theme as t0211#10). | sley-cli trace2 | **P1** |
| t1305-config-include.sh | 37 | 35 | 2 | **CODE** | `includeIf gitdir:` matching via symlink (+ icase). | sley-config / sley-cli config | **P2** |
| t4052-stat-output.sh | 91 | 89 | 2 | **CODE** | `merge --stat` vs `diff.statGraphWidth` / `statNameWidth`. Related to #135 graph-stat work. | sley-diff-merge / merge --stat | **P2** |
| t6030-bisect-porcelain.sh | 97 | 95 | 2 | **CODE** | Bare-repo bisection `--no-checkout` specified/default. | sley-cli bisect | **P2** |
| t4014-format-patch.sh | 226 | 224 | 2 | **CODE** | Multi-line / multi-line-encoded subjects. (+5 TODOs vanished — harness noise) | sley-rev format_patch | **P2** |
| t7517-per-repo-email.sh | 16 | 14 | 2 | **CODE** | Non-ff rebase refuses commits (plain + interactive) w/ per-repo email. | sley-cli rebase / identity | **P2** |
| t3502-cherry-pick-merge.sh | 12 | 10 | 2 | **CODE** | Explicit `-m1` first parent of non-merge for cherry-pick/revert. | sley-cli cherry-pick/revert | **P2** |
| t0003-attributes.sh | 55 | 54 | 1 | **CODE** | Object mode attributes for submodules. | sley-attr / submodule mode | **P2** |
| t1800-hook.sh | 92 | 91 | 1 | **CODE** | `git hook run` out-of-repo executes global hooks. | sley-cli hooks | **P2** |
| t5319-multi-pack-index.sh | 98 | 97 | 1 | **CODE** | `expire` removes repacked packs (known residual near-miss in MATRIX_SUMMARY). | sley-pack / midx | **P2** |
| t7423-submodule-symlinks.sh | 6 | 5 | 1 | **CODE** | `checkout -f --recurse-submodules` must not migrate gitdir of symlinked repo. | sley-cli submodule/checkout | **P2** |
| t0204-gettext-reencode-sanity.sh | 8 | 6 | 2 | **ENV** | gettext re-encode UTF-8↔ISO-8859-1 init messages; locale/mo availability. | harness locale / sley-i18n | P3 |
| t7813-grep-icase-iso.sh | 2 | 1 | 1 | **ENV** | `grep` PCRE + ISO-8859-1 case fold; depends on libpcre/locale. | sley-grep + host PCRE | P3 |
| t4058-diff-duplicates.sh | 16 | 16 | 0 | **ORACLE/HARNESS** | 3 known breakages **vanished** (sley ok, oracle not_ok). EXACT correctness PASS; raw FAIL exit 1. | harness / floor gate | — |
| t4072-diff-max-depth.sh | 76 | 76 | 0 | **ORACLE/HARNESS** | 26 TODOs vanished (sley ok). Not a code drop. | harness | — |
| t5610-clone-detached.sh | 13 | 13 | 0 | **ORACLE/HARNESS** | 1 known breakage vanished. | harness | — |
| t6437-submodule-merge.sh | 22 | 22 | 0 | **ORACLE/HARNESS** | 2 known breakages vanished. | harness | — |
| t1410-reflog.sh | 41 | 41 | 0 | **ORACLE/HARNESS** | 1 known breakage vanished. | harness | — |
| t3011-common-prefixes-and-directory-traversal.sh | 21 | 21 | 0 | **ORACLE/HARNESS** | 1 known breakage vanished (`ls-files -o` recurse). | harness | — |
| t1092-sparse-checkout-compatibility.sh | 111 | 110 | 0 | **ORACLE/HARNESS** | Cell vector EXACT vs oracle; 1 TODO ok (vanished) + 1 still not_ok. Floor ok-count drop is TAP counting / vanish, not new SLEY_FAILURE. | harness + floor metric | — |
| t7528-signed-commit-ssh.sh | 29 | 27 | 0 | **ORACLE/HARNESS** | EXACT correctness PASS; residual known breakages + 1 vanished. notok in summary is TODO accounting. | harness / ssh-sign TODOs | — |
| t0450-txt-doc-vs-help.sh | 794 | 881 | 0 | **ORACLE/HARNESS** | Upstream surface grew (794→896); 36 TODOs vanished + 15 still; EXACT vs oracle. Raw FAIL from known-breakage rules. | harness / help synopsis TODOs | — |
| t1517-outside-repo.sh | 84 | 337 | 0 | **ORACLE/HARNESS** | Surface grew (84→368); many -h/--help-all TODOs mixed ok/not_ok; EXACT vs oracle. | harness | — |

---

## P0 — must fix before claiming no-regression (CODE)

1. **`t3701-add-interactive` (−14)** — interactive patch edit/split regression  
   - Fails: real edit; empty-context strip; mode-only/hunk split; pathological context; `--no-advance` multi-file; etc.  
   - Owner: `crates/sley-cli` add -i/-p interactive editor  
   - Correlate: #134 (122→130) then wave1 `plumbing/add.rs` (+194) may have destabilized shared paths  

2. **`t3910-mac-os-precompose` (−12)** — `core.precomposeunicode` / NFD path handling  
   - Fails from #1 “detect if nfd needed” through checkout link nfd  
   - Owner: path normalization + worktree checkout/index  
   - Oracle green on same APFS → pure CODE  

3. **`t5813-proto-disable-ssh` (−12)** — `git+ssh://` when protocol **enabled**  
   - Only enabled clone/fetch/push cells fail (config/global allow paths)  
   - Owner: `sley-remote` protocol allow + SSH transport  

4. **`t1423-ref-backend` (−8)** — worktree + alternate ref backend URI  
   - Config/env × reftable/files × worktree cwd  
   - Owner: refs backend routing (undo/partial of #138)  

5. **`t5900-repo-selection` (−8 → 0)** — local repository path selection DWIM  
   - Total failure of `.git` suffix / bare vs non-bare prefer  
   - Owner: discovery / `enter_repo`-class path resolution  
   - High priority: complete feature disappearance  

6. **`t5521-pull-options` (−6)** — pull flag plumbing  
   - Directly in wave2 blast radius (`pull.rs` +261/−…)  
   - Owner: `sley-cli` pull  

---

## ENV list — how to make green without code (or with preflight only)

| Script | Env / preflight fix |
|--------|---------------------|
| **t0204-gettext-reencode-sanity** | Ensure full gettext/locale stack used by upstream tests (UTF-8 + ISO-8859-1). Preflight: `locale -a` contains both; `GETTEXT_POISON` off; match oracle `git` i18n build flags if sley delegates messages. If sley intentionally lacks re-encode, quarantine as known ENV skip rather than floor. |
| **t7813-grep-icase-iso** | Host must provide PCRE with ISO-8859-1 case folding compatible with git’s. Preflight: run oracle cell; if oracle pass and sley fail → CODE in `sley-grep`; if both skip → ENV. Current: oracle PASS → likely CODE-in-grep but labeled ENV because of locale/PCRE dependency; verify with `LC_ALL=en_US.ISO8859-1`. |
| **t0211 dashed helpers (partial)** | Cells expecting `remote-curl` / `http-fetch` `_run_dashed_` may need installed dashed helpers beside sley bindir. Preflight: shim or install helpers next to `SLEY_BIN`. URL-redaction cell remains CODE. |
| **SSH agent (not primary here)** | t5813 failures are enabled-path transport, not missing agent (oracle full pass). No agent preflight will green the 12 enabled cells. |
| **ORACLE/HARNESS scripts (10)** | Do **not** chase as CODE. Options: (a) teach floor/raw gate to treat “known breakage vanished” as non-regression when cells ≥ oracle; (b) count TODO `ok` vs `not_ok` separately in summary; (c) bank new floors after accepting improved behavior. |

---

## P0/P1 CODE fail-title index (for fix owners)

### P0
- **t3701:** real edit works; edit strip empty context; patch does not affect mode; stage mode but not hunk; add first line; split incomplete end; edit adding lines; pathological context (+edit); add -N then -p edit; suppressBlankEmpty split; splitting marks undecided; splitting edited hunk; selective staging `--no-advance`
- **t3910:** detect if nfd needed; diff/diff-files/diff-index/diff-tree f.Adiar; stage nfd path; log/ls-files; mv; checkout nfc/nfd/link nfd
- **t5813:** clone/fetch/push `git+ssh://` (enabled) × 4 config variants
- **t1423:** config/env × worktree × reftable/files backend (8 cells)
- **t5900:** all 8 repo-selection DWIM cells
- **t5521:** pull --no-rebase/--rebase/-v variants/-q -v/--force

### P1
- **t2071:** restore -p --source HEAD/@/HEAD^/HEAD^...; path limit HEAD^ -- dir
- **t2105:** absolute/relative gitfile submodule + update-index gitlink
- **t1020:** no file/rev ambiguity in .git / bare / symlink
- **t4255:** am(+3way) replace file with submodule → empty dir
- **t0211/t0210:** unsafe URL redaction; remote-curl/http-fetch def_params
- **t1501:** auto discovery; GIT_DIR/common vs core.worktree / GIT_WORK_TREE
- **t6301:** broken refs + NULL_SHA1 warnings
- **t2016:** checkout -p abort (HEAD/@, no staged)
- **t6133:** `^{/re}` / `@{when}` metachar DWIM
- **t0002:** update-index gitfile; setup_git_dir twice in subdir
- **t0090:** commit --interactive cache-tree partial

---

## Commit correlation (light)

| Area | Commits of interest |
|------|---------------------|
| Wave2 discovery/ceiling/open_env/pull | `d65a513c` — `discovery.rs`, `open_env.rs`, `pull.rs`, `status.rs`, remote push |
| Wave1 add/init/checkout/for-each-ref | `d11b99f3` — `plumbing/add.rs`, checkout tests, for_each_ref_helpers |
| add-interactive completion | `5506be15` / #134 (t3701 122→130) — regression risk for later add edits |
| ref-backend worktree | `b0dbcbc5` / #138 (t1423 29→36) — now 28 |
| restore/checkout -p | w49 `ead3f70e` |
| format-patch / stat | `b3489e38` / #135 |
| pull-options historical | wave-32 `94898234` |

Wave2 is the strongest suspect for **t5521**, **t1501**, **t5900** (discovery adjacency), and possibly **t1020**. Wave1 is strongest for **t3701/t0090** (add/interactive) and **t6301**. **t1423** looks like incomplete retention of #138. **t3910/t5813** may predate waves but were green on 07-10 — bisect if not explained by shared path/URL code.

---

## Recommended next fix-wave order

### Wave A — no-regression P0 (do first)
1. **t5900-repo-selection** (0/8 — highest severity per cell)  
2. **t5521-pull-options** (wave2-owned; pull.rs)  
3. **t3701-add-interactive** (+ **t0090** cache-tree interactive)  
4. **t1423-ref-backend** worktree backend routing  
5. **t5813** enabled `git+ssh://`  
6. **t3910** precomposeunicode  

### Wave B — P1 CODE cluster
1. Interactive patch: **t2071 + t2016** together  
2. Gitfile: **t2105 + t0002** together  
3. Discovery/setup: **t1020 + t1501**  
4. Pathspec DWIM: **t6133**  
5. for-each-ref errors: **t6301**  
6. Trace2 URL redaction: **t0210 + t0211** (and helper shims if needed)  
7. **t4255** am-submodule empty dir  

### Wave C — P2 polish
t1305 symlink gitdir include · t4052 merge --stat widths · t4014 multi-line subjects · t6030 bare bisect · t7517/t3502 · t0003 attrs · t1800 hooks · t5319 midx expire · t7423 submodule symlinks  

### Wave D — harness / floor (parallel, not CODE)
1. Fix comparison/floor to distinguish TODO `raw_result=ok` (vanished) vs `not_ok` (still broken).  
2. Do not treat “known breakage vanished” EXACT scripts as CODE floor drops (`t4058`, `t4072`, `t5610`, `t6437`, `t1410`, `t3011`, `t1092`, `t7528`, `t0450`, `t1517`).  
3. Re-bank floors after Wave A if ok-counts legitimately change.  
4. ENV preflight for gettext ISO + PCRE (`t0204`, `t7813`).  

---

## Counts recap

```
Hard regressions (PASS notok=0 → FAIL): 40

  CODE:           28
  ORACLE/HARNESS: 10
  ENV:             2
  UNCLEAR:         0

P0 CODE (must fix): 6 scripts, ~60 fail cells
P1 CODE:           12 scripts
P2 CODE:           10 scripts
```

**Top P0 CODE items for the next fix wave:**  
`t5900-repo-selection` (0/8), `t5521-pull-options`, `t3701-add-interactive`, `t1423-ref-backend`, `t5813-proto-disable-ssh`, `t3910-mac-os-precompose`.

**Do not spend CODE cycles on:** the 10 ORACLE/HARNESS scripts with correctness PASS / zero SLEY_FAILURE — fix the gate or accept improved TODO vanishing.

---

## Artifacts referenced

- `MATRIX_SUMMARY.md`
- `upstream-sley-summary.csv` / `upstream-oracle-summary.csv`
- `upstream-cell-comparison-summary.csv` / `upstream-cell-comparison.csv`
- `upstream-sley-cells.csv` / `upstream-sley-details.csv` / `upstream-sley-report.txt`
- Baseline: `../upstream-summary-sley-20260710.csv`
- Spot-check logs: `/tmp/sley-triage-spot/` (local hermetic re-runs)
