#!/bin/sh
#
# run-upstream-tests-waves.sh — split upstream git t/*.sh parity runs into
# parallel waves, then merge the per-wave reports/summaries.

set -u

script_path=$0
case $script_path in
    /*) ;;
    *) script_path=$(pwd)/$script_path ;;
esac
script_dir=$(CDPATH= cd -- "$(dirname -- "$script_path")" && pwd)
testkit_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
repo_root=$(CDPATH= cd -- "$testkit_dir/../.." && pwd)
runner="$script_dir/run-upstream-tests.sh"

log() { printf '%s\n' "$*" >&2; }
die() { printf 'run-upstream-tests-waves: %s\n' "$*" >&2; exit 1; }

[ -x "$runner" ] || [ -f "$runner" ] || die "serial runner missing: $runner"

DEFAULT_TESTS="
t0001-init.sh
t1006-cat-file.sh
t1007-hash-object.sh
t1300-config.sh
t1500-rev-parse.sh
t3000-ls-files-others.sh
t3103-ls-tree-misc.sh
t1401-symbolic-ref.sh
"

waves=${SLEY_UPSTREAM_WAVES:-${SLEY_TEST_WAVES:-${GIT_RS_UPSTREAM_WAVES:-4}}}
case $waves in
    *[!0-9]* | 0 | "") die "SLEY_UPSTREAM_WAVES must be a positive integer" ;;
esac
timeout_secs=${SLEY_TEST_TIMEOUT:-${GIT_RS_TEST_TIMEOUT:-240}}
case $timeout_secs in
    *[!0-9]* | "") die "SLEY_TEST_TIMEOUT must be a non-negative integer" ;;
esac

selection=$*
if [ -z "$selection" ]; then
    selection=${SLEY_TESTS:-${GIT_RS_TESTS:-$DEFAULT_TESTS}}
fi
[ -n "$selection" ] || die "no upstream scripts selected"

report=${SLEY_REPORT:-${GIT_RS_REPORT:-$repo_root/crates/sley-testkit/upstream-report.txt}}
summary=${SLEY_SUMMARY:-${GIT_RS_SUMMARY:-${report%.txt}-summary.csv}}
history=${SLEY_HISTORY:-${GIT_RS_HISTORY:-$repo_root/crates/sley-testkit/upstream-history.csv}}
timings=${SLEY_TIMINGS:-${GIT_RS_TIMINGS:-${summary%.csv}-timings.csv}}
run_label=${SLEY_RUN_LABEL:-${GIT_RS_RUN_LABEL:-}}
if [ -z "$run_label" ]; then
    run_label=$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date 2>/dev/null || printf 'unknown')
fi

tmp_root=$(mktemp -d "${TMPDIR:-/tmp}/sley-upstream-waves.XXXXXX") \
    || die "could not create temp wave root"
cleanup() {
    if [ "${SLEY_KEEP_WAVE_ARTIFACTS:-}" = "1" ]; then
        log "Keeping wave artifacts: $tmp_root"
    else
        rm -rf "$tmp_root"
    fi
}
trap cleanup EXIT INT TERM

i=1
while [ "$i" -le "$waves" ]; do
    mkdir -p "$tmp_root/wave-$i" || die "could not create wave-$i dir"
    : > "$tmp_root/wave-$i/tests"
    i=$((i + 1))
done

seen=" "
wave=1
selected_count=0
for token in $selection; do
    case " $seen " in
        *" $token "*) continue ;;
    esac
    seen="$seen$token "
    printf '%s\n' "$token" >> "$tmp_root/wave-$wave/tests"
    selected_count=$((selected_count + 1))
    wave=$((wave + 1))
    if [ "$wave" -gt "$waves" ]; then
        wave=1
    fi
done

[ "$selected_count" -gt 0 ] || die "no upstream scripts selected after dedupe"

{
    printf 'sley upstream wave test report\n'
    printf 'run label: %s\n' "$run_label"
    printf 'waves: %s\n' "$waves"
    printf 'selected scripts: %s\n' "$selected_count"
    printf 'per-script timeout: %ss\n' "$timeout_secs"
    printf 'serial runner: %s\n' "$runner"
    printf '\n'
} > "$report"

printf 'script,command,result,ok,notok,total,plan_total\n' > "$summary"
if [ ! -f "$history" ]; then
    printf 'label,script,command,result,ok,notok,total\n' > "$history"
fi
printf 'label,script,command,result,elapsed_ms,ok,notok,total,plan_total\n' > "$timings"

active_waves=""
i=1
while [ "$i" -le "$waves" ]; do
    tests=$(tr '\n' ' ' < "$tmp_root/wave-$i/tests" | sed 's/[[:space:]]*$//')
    if [ -n "$tests" ]; then
        active_waves="$active_waves $i"
        (
            SLEY_TESTS="$tests" \
            SLEY_REPORT="$tmp_root/wave-$i/report.txt" \
            SLEY_SUMMARY="$tmp_root/wave-$i/summary.csv" \
            SLEY_HISTORY="$tmp_root/wave-$i/history.csv" \
            SLEY_TIMINGS="$tmp_root/wave-$i/timings.csv" \
            SLEY_RUN_LABEL="$run_label-wave-$i" \
            SLEY_TEST_TIMEOUT="$timeout_secs" \
            sh "$runner"
        ) > "$tmp_root/wave-$i/stdout.txt" 2> "$tmp_root/wave-$i/stderr.txt" &
        printf '%s\n' "$!" > "$tmp_root/wave-$i/pid"
        log "started wave $i: $(wc -w < "$tmp_root/wave-$i/tests" | tr -d ' ') script(s)"
    fi
    i=$((i + 1))
done

failed_waves=0
for i in $active_waves; do
    pid=$(cat "$tmp_root/wave-$i/pid")
    if wait "$pid"; then
        status=0
    else
        status=$?
    fi
    printf '%s\n' "$status" > "$tmp_root/wave-$i/status"
    if [ "$status" -ne 0 ]; then
        failed_waves=$((failed_waves + 1))
    fi
    log "finished wave $i: status $status"
done

for i in $active_waves; do
    {
        printf '\n===== wave %s stdout =====\n' "$i"
        cat "$tmp_root/wave-$i/stdout.txt" 2>/dev/null || true
        printf '\n===== wave %s stderr =====\n' "$i"
        cat "$tmp_root/wave-$i/stderr.txt" 2>/dev/null || true
        if [ -f "$tmp_root/wave-$i/report.txt" ]; then
            printf '\n===== wave %s report =====\n' "$i"
            cat "$tmp_root/wave-$i/report.txt"
        fi
    } >> "$report"

    if [ -f "$tmp_root/wave-$i/summary.csv" ]; then
        tail -n +2 "$tmp_root/wave-$i/summary.csv" >> "$summary"
    fi
    if [ -f "$tmp_root/wave-$i/history.csv" ]; then
        tail -n +2 "$tmp_root/wave-$i/history.csv" >> "$history"
    fi
    if [ -f "$tmp_root/wave-$i/timings.csv" ]; then
        tail -n +2 "$tmp_root/wave-$i/timings.csv" >> "$timings"
    fi
done

{
    printf '\nPER-COMMAND PASS RATE (assertions):\n'
    printf '%-28s %-12s %6s %6s %6s  %s\n' \
        "COMMAND" "RESULT" "OK" "FAIL" "TOTAL" "PASS%"
    awk -F, '
        NR > 1 {
            pct = ($6 > 0) ? int(($4 * 100) / $6) : 0;
            printf "%-28s %-12s %6s %6s %6s  %s%%\n", $2, $3, $4, $5, $6, pct;
            scripts++;
            ok += $4;
            notok += $5;
            if ($3 == "PASS") passed++;
            else if ($3 == "TIMEOUT") timedout++;
            else failed++;
        }
        END {
            total = ok + notok;
            pct = (total > 0) ? int((ok * 100) / total) : 0;
            printf "\nASSERTION SUMMARY: ok=%d fail=%d total=%d pass=%d%%\n", ok, notok, total, pct;
            printf "SCRIPT SUMMARY: %d script(s): %d passed, %d failed, %d timed out.\n", scripts, passed, failed, timedout;
        }
    ' "$summary"
    printf 'WAVE SUMMARY: %s active wave(s), %s wave(s) returned non-zero.\n' \
        "$(printf '%s\n' "$active_waves" | wc -w | tr -d ' ')" "$failed_waves"
} | tee -a "$report"

log ""
log "Full merged report written to: $report"
log "Merged machine-readable summary: $summary"
log "Pass-rate history (appended): $history"
log "Merged per-script timings: $timings"

if awk -F, 'NR > 1 && $3 != "PASS" { exit 1 }' "$summary"; then
    exit 0
fi
exit 1
