#!/usr/bin/env bash
#
# check-parity-floors.sh — gate upstream-parity results on a per-file PASS FLOOR.
#
# Reads the per-script summary CSV written by run-upstream-tests.sh
#   columns: script,command,result,ok,notok,total,plan_total
# and verifies that every tracked t-file kept AT LEAST its recorded `ok`
# (passing-assertion) count. A net GAIN passes; only a DROP below floor fails.
#
# Usage:
#   check-parity-floors.sh <summary.csv>
#
# Floors recorded 2026-06-10 against base b069dbe + git 2.54.0 (GIT_TEST_DEFAULT_HASH=sha1).
# Round-2 floors (sequencer/graph/bitmaps) measured 2026-06-11 at the
# integ/round2 tip (epic/sley-sequencer-c + epic/sley-graph-c +
# epic/sley-bitmaps-a merged onto baac87c).
# Round-3 floors (rebase-i/bitmaps-b/diff-formats) measured 2026-06-11 at the
# integ/round3 tip (epic/sley-rebase-i + epic/sley-bitmaps-b +
# epic/sley-diff-formats merged onto 04f10f8).
# Round-4a floors (fetch-pack/shallow + userdiff) measured 2026-06-12 at the
# integ/round4a tip (epic/sley-fetchpack + epic/sley-userdiff merged onto
# b1b606a).
# Round-4b floors (gitlink stage-1: submodule basics/status/diff) measured
# 2026-06-12 at the integ/round4b tip (epic/sley-gitlink-a merged onto
# e08520c). t0008-ignores.sh deadlocks on its last cell ('streaming support
# for --stdin', pre-existing check-ignore buffering bug, sley#44) until the
# harness timeout; the summary row still records ok=284 (counts parse from
# the captured TAP before the hang), so the floor is valid — but the run
# needs SLEY_TEST_TIMEOUT high enough (900s) to reach cell 284 first.
# Round-5c floors (hooks-execution epic + stdin-streaming class) measured
# 2026-06-13 at the integ/round5c tip (epic/sley-hooks +
# fix/stdin-streaming-class merged onto 5a81341, @ 2b5d2d6). Gains over the
# round-5c hooks-layer baseline (ffbdc08): t5407-post-rewrite-hook 3->10,
# t5571-pre-push-hook 6->11, t0008-ignores 284->306 (still deadlocks on its
# last cell, sley#44 — 306 cells record before the 900s timeout kill), plus the
# new t1400-update-ref.sh floored at 175. t1800-hook stays at 55 (already a
# full pass since the hooks-layer landing).
# format-patch wave (2026-06-14, parity/fmtpatch): t4014-format-patch added at
# 96 (was 38 — banked the To/Cc/extra-header, --from/format.from in-body,
# --signature/--signature-file, --signoff trailer-block, and --rfc clusters).
# Same change lifted t4202-log 55->56 (shared --from/identity-header path), so
# its floor is raised to 56. No diff/log floor regressed (t4013/t4015/t4018/
# t4052/t4034/t4045/t4047/t4027 all held; render.rs untouched).
# Raise a floor only after a real, sustained gain lands; never lower one.

set -euo pipefail

summary=${1:?usage: check-parity-floors.sh <summary.csv>}

if [ ! -f "$summary" ]; then
    echo "FAIL: summary CSV not found: $summary" >&2
    echo "      (did run-upstream-tests.sh run? it writes this file)" >&2
    exit 1
fi

# script -> floor (minimum acceptable ok-assertion count).
declare -A FLOOR=(
    [t0001-init.sh]=102
    [t1006-cat-file.sh]=290
    [t1007-hash-object.sh]=40
    [t1300-config.sh]=497
    [t1400-update-ref.sh]=175
    [t1401-symbolic-ref.sh]=25
    [t1500-rev-parse.sh]=81
    [t2400-worktree-add.sh]=165
    [t3070-wildmatch.sh]=1861
    [t6300-for-each-ref.sh]=358
    [t7004-tag.sh]=159
    [t3200-branch.sh]=117
    [t0027-auto-crlf.sh]=2281
    [t2107-update-index-basic.sh]=10
    [t7810-grep.sh]=228
    [t3301-notes.sh]=144
    [t1461-refs-list.sh]=358
    [t1462-refs-exists.sh]=12
    [t1510-repo-setup.sh]=109
    [t6423-merge-rename-directories.sh]=14
    [t3501-revert-cherry-pick.sh]=21
    [t3502-cherry-pick-merge.sh]=12
    [t3505-cherry-pick-empty.sh]=17
    [t3507-cherry-pick-conflict.sh]=44
    [t3510-cherry-pick-sequence.sh]=52
    [t4214-log-graph-octopus.sh]=17
    [t4215-log-skewed-merges.sh]=9
    [t6030-bisect-porcelain.sh]=95
    [t5310-pack-bitmaps.sh]=218
    [t5326-multi-pack-bitmaps.sh]=336
    [t6113-rev-list-bitmap-filters.sh]=13
    [t1800-hook.sh]=55
    [t2020-checkout-detach.sh]=16
    [t6003-rev-list-topo-order.sh]=36
    [t6012-rev-list-simplify.sh]=9
    [t4205-log-pretty-formats.sh]=108
    [t4216-log-bloom.sh]=112
    [t5318-commit-graph.sh]=38
    [t3432-rebase-fast-forward.sh]=144
    [t4202-log.sh]=56
    [t3000-ls-files-others.sh]=15
    [t3103-ls-tree-misc.sh]=10
    [t3403-rebase-skip.sh]=16
    [t3404-rebase-interactive.sh]=36
    [t3406-rebase-message.sh]=9
    [t3418-rebase-continue.sh]=11
    [t3420-rebase-autostash.sh]=30
    [t5327-multi-pack-bitmaps-rev.sh]=308
    [t5332-multi-pack-reuse.sh]=9
    [t4013-diff-various.sh]=133
    [t4014-format-patch.sh]=96
    [t4052-stat-output.sh]=71
    [t4045-diff-relative.sh]=29
    [t4047-diff-dirstat.sh]=41
    [t4015-diff-whitespace.sh]=55
    [t4018-diff-funcname.sh]=287
    [t4034-diff-words.sh]=64
    [t5407-post-rewrite-hook.sh]=10
    [t5500-fetch-pack.sh]=356
    [t5571-pre-push-hook.sh]=11
    [t5537-fetch-shallow.sh]=10
    [t0008-ignores.sh]=306
    [t7400-submodule-basic.sh]=70
    [t7506-status-submodule.sh]=28
    [t7508-status.sh]=66
    [t4027-diff-submodule.sh]=18
)

fail=0
seen=""

# Skip the header row; read the columns we care about.
while IFS=, read -r script command result ok notok total plan_total; do
    [ "$script" = "script" ] && continue          # header
    [ -z "${script:-}" ] && continue              # blank line
    floor=${FLOOR[$script]:-}
    if [ -z "$floor" ]; then
        # An untracked script appeared in the output — informational only.
        echo "note: $script not in floor table (ok=$ok) — not gated"
        continue
    fi
    seen="$seen $script"
    ok=${ok:-0}
    if ! [ "$ok" -ge 0 ] 2>/dev/null; then
        echo "FAIL: $script: unparseable ok count '$ok'" >&2
        fail=1
        continue
    fi
    if [ "$ok" -lt "$floor" ]; then
        echo "FAIL: $script: ok=$ok dropped below floor=$floor (result=$result)" >&2
        fail=1
    elif [ "$ok" -gt "$floor" ]; then
        echo "GAIN: $script: ok=$ok (floor=$floor) — +$((ok - floor)); bump the floor"
    else
        echo "ok:   $script: ok=$ok == floor=$floor"
    fi
done < "$summary"

# Every tracked file must have appeared in the summary. A missing file means
# the script never ran (build break, wrong selection) — treat as a failure so
# a silently-skipped t-file can't pass the gate.
for script in "${!FLOOR[@]}"; do
    case " $seen " in
        *" $script "*) ;;
        *)
            echo "FAIL: $script: tracked file absent from summary (did it run?)" >&2
            fail=1
            ;;
    esac
done

if [ "$fail" -ne 0 ]; then
    echo "PARITY FLOOR GATE: FAILED — at least one t-file regressed below floor." >&2
    exit 1
fi
echo "PARITY FLOOR GATE: PASSED — all tracked t-files at or above floor."
