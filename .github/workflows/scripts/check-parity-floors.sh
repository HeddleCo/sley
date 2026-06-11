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
    [t0001-init.sh]=99
    [t1006-cat-file.sh]=285
    [t1007-hash-object.sh]=40
    [t1300-config.sh]=445
    [t1401-symbolic-ref.sh]=25
    [t1500-rev-parse.sh]=80
    [t2400-worktree-add.sh]=158
    [t3070-wildmatch.sh]=1861
    [t6300-for-each-ref.sh]=337
    [t7004-tag.sh]=150
    [t3200-branch.sh]=70
    [t0027-auto-crlf.sh]=2149
    [t2107-update-index-basic.sh]=9
    [t7810-grep.sh]=228
    [t3301-notes.sh]=144
    [t1461-refs-list.sh]=337
    [t1462-refs-exists.sh]=11
    [t1510-repo-setup.sh]=109
    [t6423-merge-rename-directories.sh]=14
    [t3501-revert-cherry-pick.sh]=5
    [t2020-checkout-detach.sh]=9
    [t6003-rev-list-topo-order.sh]=36
    [t6012-rev-list-simplify.sh]=8
    [t4205-log-pretty-formats.sh]=107
    [t4202-log.sh]=34
    [t3000-ls-files-others.sh]=15
    [t3103-ls-tree-misc.sh]=10
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
