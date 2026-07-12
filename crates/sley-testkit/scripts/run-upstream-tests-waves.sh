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

waves=${SLEY_UPSTREAM_WAVES:-${SLEY_TEST_WAVES:-4}}
case $waves in
    *[!0-9]* | 0 | "") die "SLEY_UPSTREAM_WAVES must be a positive integer" ;;
esac
timeout_secs=${SLEY_TEST_TIMEOUT:-240}
case $timeout_secs in
    *[!0-9]* | "") die "SLEY_TEST_TIMEOUT must be a non-negative integer" ;;
esac

selection=$*
if [ -z "$selection" ]; then
    selection=${SLEY_TESTS:-curated}
fi
if [ "$selection" = "curated" ]; then
    selection=$(sh "$runner" --list-curated) || die "curated manifest validation/listing failed"
fi
[ -n "$selection" ] || die "no upstream scripts selected"

report=${SLEY_REPORT:-$repo_root/crates/sley-testkit/upstream-report.txt}
summary=${SLEY_SUMMARY:-${report%.txt}-summary.csv}
history=${SLEY_HISTORY:-$repo_root/crates/sley-testkit/upstream-history.csv}
timings=${SLEY_TIMINGS:-${summary%.csv}-timings.csv}
cells=${SLEY_CELLS:-${summary%.csv}-cells.csv}
details=${SLEY_DETAILS:-${summary%.csv}-details.csv}
comparison=${SLEY_COMPARISON:-${summary%.csv}-comparison.csv}
comparison_summary=${SLEY_COMPARISON_SUMMARY:-${comparison%.csv}-summary.csv}
metadata=${SLEY_METADATA:-${summary%.csv}-metadata.tsv}
run_label=${SLEY_RUN_LABEL:-}
if [ -z "$run_label" ]; then
    run_label=$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date 2>/dev/null || printf 'unknown')
fi

# Artifact paths are frequently rooted in a fresh certification directory.
# Create every parent before launching workers so a missing report directory
# cannot start an expensive run whose merged outputs are impossible to write.
for artifact in "$report" "$summary" "$history" "$timings" "$cells" "$details" "$comparison" "$comparison_summary" "$metadata"; do
    artifact_parent=$(dirname -- "$artifact")
    mkdir -p "$artifact_parent" || die "could not create artifact directory: $artifact_parent"
done

metadata_value() {
    printf '%s' "$1" | tr '\t\r\n' '   '
}
metadata_hash=${SLEY_DEFAULT_HASH:-${GIT_TEST_DEFAULT_HASH:-sha1}}
metadata_uname=$(uname -s 2>/dev/null || printf unknown)
case ${OS:-}:$metadata_uname in
    *Windows*:* | *:MINGW* | *:MSYS* | *:CYGWIN*) metadata_platform=windows ;;
    *:Darwin) metadata_platform=macos ;;
    *:Linux) metadata_platform=linux ;;
    *) metadata_platform=$(printf '%s' "$metadata_uname" | tr '[:upper:]' '[:lower:]') ;;
esac
metadata_upstream_t=${SLEY_UPSTREAM_T:-${GIT_SRC_DIR:+$GIT_SRC_DIR/t}}
metadata_upstream_root=$(dirname -- "${metadata_upstream_t:-unknown/t}")
metadata_upstream_commit=$(git -C "$metadata_upstream_root" rev-parse HEAD 2>/dev/null || printf unknown)
metadata_candidate_commit=${SLEY_CANDIDATE_COMMIT:-$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || printf unknown)}
if metadata_candidate_status=$(git -C "$repo_root" status --porcelain --untracked-files=normal 2>/dev/null); then
    if [ -n "$metadata_candidate_status" ]; then metadata_candidate_tree_state=dirty; else metadata_candidate_tree_state=clean; fi
else
    metadata_candidate_tree_state=unknown
fi
if metadata_upstream_status=$(git -C "$metadata_upstream_root" status --porcelain --untracked-files=normal 2>/dev/null); then
    if [ -n "$metadata_upstream_status" ]; then metadata_upstream_tree_state=dirty; else metadata_upstream_tree_state=clean; fi
else
    metadata_upstream_tree_state=unknown
fi
metadata_target=${SLEY_TEST_TARGET:-sley}
if [ "$metadata_target" = "oracle" ]; then
    metadata_binary=${SLEY_ORACLE_BIN:-${SLEY_TEST_GIT:-unknown}}
else
    metadata_binary=${SLEY_BIN:-unknown}
fi
metadata_version=$("$metadata_binary" --version 2>/dev/null | head -n 1 || true)
metadata_binary_checksum=$(cksum "$metadata_binary" 2>/dev/null | awk '{ print $1 ":" $2; exit }')
metadata_binary_checksum=${metadata_binary_checksum:-unknown}
metadata_manifest=${SLEY_UPSTREAM_MANIFEST:-$testkit_dir/upstream-manifest.tsv}
metadata_manifest_checksum=$(cksum "$metadata_manifest" 2>/dev/null | awk '{ print $1 ":" $2; exit }')
metadata_manifest_checksum=${metadata_manifest_checksum:-unknown}
metadata_arch=$(uname -m 2>/dev/null || printf unknown)
metadata_selection_count=$(printf '%s\n' $selection | awk 'NF { count++ } END { print count + 0 }')
metadata_selection_checksum=$(printf '%s\n' $selection | cksum | awk '{ print $1 ":" $2; exit }')
{
    printf 'schema\tsley-upstream-run-metadata-v1\n'
    printf 'run_label\t%s\n' "$(metadata_value "$run_label")"
    printf 'target\t%s\n' "$(metadata_value "$metadata_target")"
    printf 'candidate_commit\t%s\n' "$(metadata_value "$metadata_candidate_commit")"
    printf 'candidate_tree_state\t%s\n' "$(metadata_value "$metadata_candidate_tree_state")"
    printf 'target_binary\t%s\n' "$(metadata_value "$metadata_binary")"
    printf 'target_binary_checksum\t%s\n' "$(metadata_value "$metadata_binary_checksum")"
    printf 'target_version\t%s\n' "$(metadata_value "$metadata_version")"
    printf 'upstream_commit\t%s\n' "$(metadata_value "$metadata_upstream_commit")"
    printf 'upstream_tree_state\t%s\n' "$(metadata_value "$metadata_upstream_tree_state")"
    printf 'upstream_t\t%s\n' "$(metadata_value "$metadata_upstream_t")"
    printf 'manifest\t%s\n' "$(metadata_value "$metadata_manifest")"
    printf 'manifest_checksum\t%s\n' "$(metadata_value "$metadata_manifest_checksum")"
    printf 'platform\t%s\n' "$(metadata_value "$metadata_platform")"
    printf 'architecture\t%s\n' "$(metadata_value "$metadata_arch")"
    printf 'hash\t%s\n' "$(metadata_value "$metadata_hash")"
    printf 'selection_count\t%s\n' "$(metadata_value "$metadata_selection_count")"
    printf 'selection_checksum\t%s\n' "$(metadata_value "$metadata_selection_checksum")"
} > "$metadata"

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
    printf 'metadata: %s\n' "$metadata"
    printf '\n'
} > "$report"

printf 'script,command,result,ok,notok,total,plan_total\n' > "$summary"
if [ ! -f "$history" ]; then
    printf 'label,script,command,result,ok,notok,total\n' > "$history"
fi
printf 'label,script,command,result,elapsed_ms,ok,notok,total,plan_total\n' > "$timings"
printf 'target,script,cell,status,raw_result,directive,description\n' > "$cells"
printf 'target,script,result,exit_code,pass,fail,todo,skip,total_cells,plan_total,abort,timeout,missing_cells,extra_cells\n' > "$details"
if [ -n "${SLEY_ORACLE_CELLS:-}" ]; then
    printf 'script,cell,oracle_status,sley_status,comparison\n' > "$comparison"
    printf 'script,oracle_result,sley_result,oracle_cells,sley_cells,cell_vector,correctness,unexpected_sley_skips,missing_sley_cells,extra_sley_cells,performance_eligible,performance_comparison\n' > "$comparison_summary"
fi

active_waves=""
i=1
while [ "$i" -le "$waves" ]; do
    tests=$(tr '\n' ' ' < "$tmp_root/wave-$i/tests" | sed 's/[[:space:]]*$//')
    if [ -n "$tests" ]; then
        active_waves="$active_waves $i"
        (
            export SLEY_TESTS="$tests"
            export SLEY_REPORT="$tmp_root/wave-$i/report.txt"
            export SLEY_SUMMARY="$tmp_root/wave-$i/summary.csv"
            export SLEY_HISTORY="$tmp_root/wave-$i/history.csv"
            export SLEY_TIMINGS="$tmp_root/wave-$i/timings.csv"
            export SLEY_CELLS="$tmp_root/wave-$i/cells.csv"
            export SLEY_DETAILS="$tmp_root/wave-$i/details.csv"
            export SLEY_METADATA="$tmp_root/wave-$i/metadata.tsv"
            export SLEY_RUN_LABEL="$run_label-wave-$i"
            export SLEY_TEST_TIMEOUT="$timeout_secs"
            if [ -n "${SLEY_ORACLE_CELLS:-}" ]; then
                awk -F, -v tests=" $tests " '
                    FNR == 1 { print; next }
                    {
                        script = $2
                        gsub(/^"|"$/, "", script)
                        if (index(tests, " " script " ")) print
                    }
                ' "$SLEY_ORACLE_CELLS" > "$tmp_root/wave-$i/oracle-cells.csv"
                export SLEY_ORACLE_CELLS="$tmp_root/wave-$i/oracle-cells.csv"
                if [ -n "${SLEY_ORACLE_DETAILS:-}" ] && [ -f "$SLEY_ORACLE_DETAILS" ]; then
                    awk -F, -v tests=" $tests " 'FNR == 1 || index(tests, " " $2 " ") { print }' \
                        "$SLEY_ORACLE_DETAILS" > "$tmp_root/wave-$i/oracle-details.csv"
                    export SLEY_ORACLE_DETAILS="$tmp_root/wave-$i/oracle-details.csv"
                fi
                export SLEY_COMPARISON="$tmp_root/wave-$i/comparison.csv"
                export SLEY_COMPARISON_SUMMARY="$tmp_root/wave-$i/comparison-summary.csv"
            fi
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
    if [ -f "$tmp_root/wave-$i/cells.csv" ]; then
        tail -n +2 "$tmp_root/wave-$i/cells.csv" >> "$cells"
    fi
    if [ -f "$tmp_root/wave-$i/details.csv" ]; then
        tail -n +2 "$tmp_root/wave-$i/details.csv" >> "$details"
    fi
    if [ -f "$tmp_root/wave-$i/comparison.csv" ]; then
        tail -n +2 "$tmp_root/wave-$i/comparison.csv" >> "$comparison"
    fi
    if [ -f "$tmp_root/wave-$i/comparison-summary.csv" ]; then
        tail -n +2 "$tmp_root/wave-$i/comparison-summary.csv" >> "$comparison_summary"
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
            else if ($3 == "SKIP") skipped++;
            else if ($3 == "TIMEOUT") timedout++;
            else if ($3 == "ABORT") aborted++;
            else failed++;
        }
        END {
            total = ok + notok;
            pct = (total > 0) ? int((ok * 100) / total) : 0;
            printf "\nASSERTION SUMMARY: ok=%d fail=%d total=%d pass=%d%%\n", ok, notok, total, pct;
            printf "SCRIPT SUMMARY: %d script(s): %d passed, %d skipped, %d failed, %d aborted, %d timed out.\n", scripts, passed, skipped, failed, aborted, timedout;
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
log "Merged exact TAP cells: $cells"
log "Merged per-script classifications: $details"
log "Run identity metadata: $metadata"
if [ -n "${SLEY_ORACLE_CELLS:-}" ]; then
    log "Merged oracle/Sley cell comparison: $comparison"
    log "Merged oracle/Sley comparison summary: $comparison_summary"
fi

if awk -F, 'NR > 1 && $3 != "PASS" && $3 != "SKIP" { exit 1 }' "$summary"; then
    exit 0
fi
exit 1
