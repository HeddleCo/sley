#!/bin/sh
#
# run-upstream-tests.sh — run UPSTREAM git's own t/*.sh test suite against the
# git-rs binary, as the ultimate parity oracle.
#
# Upstream git ships a TAP-emitting shell test framework (t/test-lib.sh plus
# t/tNNNN-*.sh scripts). test-lib.sh can exercise an *externally installed* git
# via the GIT_TEST_INSTALLED environment variable, which must point at a
# directory ("bindir") containing a working `git` executable. This script
# builds such a bindir whose `git` is a shim that execs the git-rs binary, then
# runs a configurable subset of the upstream scripts against it and aggregates
# the results.
#
# ---------------------------------------------------------------------------
# USAGE
#
#   crates/git-testkit/scripts/run-upstream-tests.sh [SCRIPT...]
#
# Required: point the runner at an upstream git source checkout's t/ directory
# using ONE of:
#
#   GIT_RS_UPSTREAM_T   absolute path to the upstream git "t/" directory
#   GIT_SRC_DIR         absolute path to a git source ROOT (we use $GIT_SRC_DIR/t)
#
# IMPORTANT: upstream test-lib.sh sources "$GIT_BUILD_DIR/GIT-BUILD-OPTIONS"
# (where GIT_BUILD_DIR is the parent of t/) and aborts if it is missing. That
# file is produced by *configuring/building* the git source tree. So the t/
# directory must come from a BUILT git checkout. The quickest way to get one:
#
#       git clone --depth=1 https://github.com/git/git /tmp/git-src
#       cd /tmp/git-src && make GIT-BUILD-OPTIONS
#       # (a full `make` also works but is not required just to get the file)
#       export GIT_SRC_DIR=/tmp/git-src
#
# Then, from the git-rs repo root:
#
#       crates/git-testkit/scripts/run-upstream-tests.sh
#
# Optional environment variables:
#
#   GIT_RS_BIN          absolute path to the git-rs binary. If unset we try
#                       $CARGO_BIN_EXE_git-rs, then target/debug/git-rs, and
#                       finally `cargo build -p git-cli`.
#   GIT_RS_TESTS        space-separated default script list (overrides the
#                       built-in default subset). Positional args override this.
#   GIT_RS_TEST_TIMEOUT per-script timeout in seconds (default 120). 0 disables.
#                       Falls back to a Perl alarm(2) wrapper when neither
#                       timeout(1) nor gtimeout(1) is on PATH, so a hanging
#                       command (e.g. `rev-parse --short=N`) cannot stall the
#                       whole batch.
#   GIT_RS_REPORT       path for the human-readable report file
#                       (default: crates/git-testkit/upstream-report.txt).
#   GIT_RS_SUMMARY      path for the machine-readable per-command CSV summary
#                       (default: <report>-summary.csv). Columns:
#                       script,command,result,ok,notok,total,plan_total.
#   GIT_RS_HISTORY      append-only per-command pass-rate history CSV
#                       (default: crates/git-testkit/upstream-history.csv).
#                       Columns: label,script,command,result,ok,notok,total.
#   GIT_RS_RUN_LABEL    label recorded in the report/history for this run (e.g.
#                       a git short-SHA or tag). Defaults to a UTC timestamp.
#                       The library never reads a clock; pass this to make runs
#                       reproducibly labelled.
#   GIT_RS_DEFAULT_HASH hash algorithm primed into test-lib's test_oid database
#                       (default: sha1; or sha256). Without it test-lib aborts
#                       scripts with "BUG: undefined key" and pollutes results.
#   GIT_RS_TEST_OPTS    extra options forwarded to each upstream script
#                       (e.g. "--verbose" or "-x"). --no-bin-wrappers is always
#                       supplied because GIT_TEST_INSTALLED has no bin-wrappers.
#
# Each SCRIPT argument may be a command name ("config", "cat-file", "ls-tree"),
# a bare basename ("t0001-init.sh"), a numeric prefix ("t0001"), or a glob
# ("t13*"); it is resolved against the upstream t/ directory. Command names map
# to the foundational subset (init, cat-file, hash-object, config, rev-parse,
# ls-files, ls-tree, symbolic-ref), so targeting one command is easy:
#
#       run-upstream-tests.sh config        # just t1300-config.sh
#       run-upstream-tests.sh cat-file ls-tree
#
# Exit status is 0 only if every selected script passed.
# ---------------------------------------------------------------------------

set -u

# --- Default subset -------------------------------------------------------
#
# Chosen to target commands git-rs already implements (init, hash-object,
# cat-file, config, rev-parse, ls-files, ls-tree, symbolic-ref, update-ref),
# while staying small so a run finishes quickly and a hang in one script does
# not stall the batch. Names are exact upstream filenames as of git master.
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

# --- Command-name aliases -------------------------------------------------
#
# So a caller can target a single command by its git subcommand name rather
# than memorising script numbers, e.g.
#
#       run-upstream-tests.sh config
#       run-upstream-tests.sh cat-file ls-tree
#
# Each alias maps to one foundational script. Tokens that are not aliases fall
# through to the normal basename/prefix/glob resolution against the upstream t/
# directory, so "t1300", "t13*", and "t1300-config.sh" all still work.
command_alias() {
    case $1 in
        init)         printf 't0001-init.sh\n' ;;
        cat-file)     printf 't1006-cat-file.sh\n' ;;
        hash-object)  printf 't1007-hash-object.sh\n' ;;
        config)       printf 't1300-config.sh\n' ;;
        rev-parse)    printf 't1500-rev-parse.sh\n' ;;
        ls-files)     printf 't3000-ls-files-others.sh\n' ;;
        ls-tree)      printf 't3103-ls-tree-misc.sh\n' ;;
        symbolic-ref) printf 't1401-symbolic-ref.sh\n' ;;
        *) return 1 ;;
    esac
}

# --- Locate the git-rs repo root and this script --------------------------
script_path=$0
case $script_path in
    /*) ;;
    *) script_path=$(pwd)/$script_path ;;
esac
script_dir=$(CDPATH= cd -- "$(dirname -- "$script_path")" && pwd)
# scripts/ -> git-testkit -> crates -> repo root
testkit_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
repo_root=$(CDPATH= cd -- "$testkit_dir/../.." && pwd)

log() { printf '%s\n' "$*" >&2; }
die() { printf 'run-upstream-tests: %s\n' "$*" >&2; exit 1; }

# --- Resolve the upstream t/ directory ------------------------------------
upstream_t=""
if [ -n "${GIT_RS_UPSTREAM_T:-}" ]; then
    upstream_t=$GIT_RS_UPSTREAM_T
elif [ -n "${GIT_SRC_DIR:-}" ]; then
    upstream_t=$GIT_SRC_DIR/t
fi

if [ -z "$upstream_t" ]; then
    log "SKIP: no upstream git t/ directory configured."
    log ""
    log "Set GIT_RS_UPSTREAM_T to the upstream git 't/' directory, or set"
    log "GIT_SRC_DIR to a git source root (we then use \$GIT_SRC_DIR/t)."
    log ""
    log "To obtain a usable checkout:"
    log "    git clone --depth=1 https://github.com/git/git /tmp/git-src"
    log "    cd /tmp/git-src && make GIT-BUILD-OPTIONS"
    log "    export GIT_SRC_DIR=/tmp/git-src"
    log ""
    log "Then re-run this script from the git-rs repo root."
    # Exit 0: an unconfigured environment is a skip, not a failure, so callers
    # (and `cargo test`) stay green.
    exit 0
fi

if [ ! -d "$upstream_t" ]; then
    die "upstream t/ directory does not exist: $upstream_t"
fi
upstream_t=$(CDPATH= cd -- "$upstream_t" && pwd)
if [ ! -f "$upstream_t/test-lib.sh" ]; then
    die "no test-lib.sh under $upstream_t (is this really git's t/ directory?)"
fi

build_dir=$(dirname -- "$upstream_t")
if [ ! -f "$build_dir/GIT-BUILD-OPTIONS" ]; then
    log "WARNING: $build_dir/GIT-BUILD-OPTIONS is missing."
    log "         Upstream test-lib.sh requires it and will abort each script."
    log "         Run 'make GIT-BUILD-OPTIONS' in $build_dir first."
fi

# --- Resolve the git-rs binary --------------------------------------------
git_rs_bin=""
# NOTE: CARGO_BIN_EXE_git-rs contains a hyphen, which is not a valid POSIX
# shell identifier, so it cannot be read via ${CARGO_BIN_EXE_git-rs}. Read it
# from the environment with printenv instead.
cargo_bin_exe=$(printenv CARGO_BIN_EXE_git-rs 2>/dev/null || true)
if [ -n "${GIT_RS_BIN:-}" ]; then
    git_rs_bin=$GIT_RS_BIN
elif [ -n "$cargo_bin_exe" ]; then
    git_rs_bin=$cargo_bin_exe
elif [ -x "$repo_root/target/debug/git-rs" ]; then
    git_rs_bin=$repo_root/target/debug/git-rs
fi

if [ -z "$git_rs_bin" ] || [ ! -x "$git_rs_bin" ]; then
    log "git-rs binary not found; building with 'cargo build -p git-cli'..."
    ( cd "$repo_root" && cargo build -p git-cli ) || die "cargo build -p git-cli failed"
    git_rs_bin=$repo_root/target/debug/git-rs
fi
[ -x "$git_rs_bin" ] || die "git-rs binary still not executable: $git_rs_bin"
# Absolutize.
case $git_rs_bin in
    /*) ;;
    *) git_rs_bin=$(pwd)/$git_rs_bin ;;
esac
log "git-rs binary: $git_rs_bin"

# --- Build the shim bindir ------------------------------------------------
#
# test-lib.sh runs `$GIT_TEST_INSTALLED/git --exec-path` early and aborts if it
# fails, so the shim must answer the introspection flags itself; everything else
# is delegated to git-rs. We also export GIT_RS_BIN inside the shim so its value
# is visible regardless of how the shim is invoked.
bindir=$(mktemp -d "${TMPDIR:-/tmp}/git-rs-upstream-bindir.XXXXXX") \
    || die "could not create temp bindir"
cleanup() { rm -rf "$bindir"; }
trap cleanup EXIT INT TERM

cat > "$bindir/git" <<SHIM
#!/bin/sh
# Auto-generated git-rs shim for upstream test-lib.sh (GIT_TEST_INSTALLED).
GIT_RS_BIN='$git_rs_bin'
SHIM_DIR='$bindir'
case "\${1:-}" in
    --exec-path)
        # test-lib.sh: GIT_EXEC_PATH=\$(\$GIT_TEST_INSTALLED/git --exec-path)
        printf '%s\n' "\$SHIM_DIR"
        exit 0
        ;;
    --man-path|--html-path|--info-path)
        printf '%s\n' "\$SHIM_DIR"
        exit 0
        ;;
esac
exec "\$GIT_RS_BIN" "\$@"
SHIM
chmod +x "$bindir/git"

# Sanity-check the shim answers --exec-path (mirrors test-lib.sh's probe).
if ! "$bindir/git" --exec-path >/dev/null 2>&1; then
    die "git-rs shim failed its --exec-path self-check"
fi

# --- Select scripts -------------------------------------------------------
selection=$*
if [ -z "$selection" ]; then
    selection=${GIT_RS_TESTS:-$DEFAULT_TESTS}
fi

resolve_one() {
    # Resolve a user token to an existing script basename under upstream_t.
    token=$1
    # Command-name alias first (e.g. "config" -> "t1300-config.sh"), so a
    # caller can target a single command without knowing script numbers.
    if aliased=$(command_alias "$token"); then
        if [ -f "$upstream_t/$aliased" ]; then
            printf '%s\n' "$aliased"
            return 0
        fi
        # Aliased script not present in this checkout: fall through so the
        # caller still gets a "no script matched" warning rather than silence.
    fi
    # Exact match next.
    if [ -f "$upstream_t/$token" ]; then
        printf '%s\n' "$token"
        return 0
    fi
    # Glob / prefix match (e.g. "t0001" or "t13*").
    matched=""
    for cand in "$upstream_t"/$token*.sh; do
        [ -f "$cand" ] || continue
        matched="$matched $(basename -- "$cand")"
    done
    if [ -n "$matched" ]; then
        for m in $matched; do printf '%s\n' "$m"; done
        return 0
    fi
    return 1
}

scripts=""
missing=""
for token in $selection; do
    if resolved=$(resolve_one "$token"); then
        scripts="$scripts $resolved"
    else
        missing="$missing $token"
    fi
done

if [ -n "$missing" ]; then
    log "WARNING: no upstream script matched:$missing"
fi
if [ -z "$scripts" ]; then
    die "no upstream scripts selected (looked in $upstream_t)"
fi

# --- Timeout helper -------------------------------------------------------
#
# A per-script wall-clock cap matters here: some git-rs commands still hang
# (e.g. `rev-parse --short=N` for N >= the hash length spins forever), and
# without a cap a single hang stalls the whole batch. We prefer GNU
# timeout(1)/gtimeout(1) when present; otherwise we fall back to a small Perl
# `alarm` wrapper (perl is required by upstream test-lib.sh, so it is always
# available in a usable checkout). The fallback's exit status for a timeout is
# 142 (128 + SIGALRM=14); we normalise both 124 (GNU timeout) and 142 to
# "TIMEOUT" below.
timeout_secs=${GIT_RS_TEST_TIMEOUT:-120}
timeout_kind="none"
timeout_cmd=""
timeout_perl=""
if [ "$timeout_secs" != "0" ]; then
    if command -v timeout >/dev/null 2>&1; then
        timeout_cmd="timeout ${timeout_secs}s"
        timeout_kind="gnu"
    elif command -v gtimeout >/dev/null 2>&1; then
        timeout_cmd="gtimeout ${timeout_secs}s"
        timeout_kind="gnu"
    elif command -v perl >/dev/null 2>&1; then
        timeout_perl=$(command -v perl)
        timeout_kind="perl"
        log "NOTE: no timeout(1)/gtimeout(1); using a Perl alarm(${timeout_secs}s) fallback."
    else
        log "NOTE: no timeout(1)/gtimeout(1)/perl found; running without per-script timeout."
    fi
fi

# run_with_timeout CMD... — run CMD under whichever timeout mechanism we found.
#
# The Perl fallback forks the child into its own process group and, on
# alarm, kills that whole group. This matters because a git-rs command can spin
# at 100% CPU (e.g. `rev-parse --short=N`); a naive `alarm; exec` would let the
# parent die while the spinning grandchild lived on and accumulated across the
# batch. Exit status 142 (128 + SIGALRM) signals a timeout to the caller.
run_with_timeout() {
    case $timeout_kind in
        gnu)  $timeout_cmd "$@" ;;
        perl)
            "$timeout_perl" -e '
                use POSIX qw(setpgid);
                my $secs = shift @ARGV;
                my $pid = fork();
                die "fork: $!" unless defined $pid;
                if ($pid == 0) {
                    setpgid(0, 0);
                    exec @ARGV or do { warn "exec: $!"; POSIX::_exit(127); };
                }
                setpgid($pid, $pid);
                local $SIG{ALRM} = sub { kill("KILL", -$pid); waitpid($pid, 0); exit 142; };
                alarm $secs;
                waitpid($pid, 0);
                alarm 0;
                my $st = $?;
                exit($st & 127 ? 128 + ($st & 127) : ($st >> 8));
            ' "$timeout_secs" "$@" ;;
        *)    "$@" ;;
    esac
}

report=${GIT_RS_REPORT:-$repo_root/crates/git-testkit/upstream-report.txt}
extra_opts=${GIT_RS_TEST_OPTS:-}

# Machine-readable per-command summary (one CSV row per script):
#   script,command,result,ok,notok,total,plan_total
# Default lives next to the human report.
summary=${GIT_RS_SUMMARY:-${report%.txt}-summary.csv}

# Append-only per-command pass-rate history, so trends are visible across runs.
# Each row: label,script,command,result,ok,notok,total. The label is supplied
# by the caller (GIT_RS_RUN_LABEL) so the library never has to call a clock;
# when unset we fall back to a UTC timestamp from date(1) at the shell layer
# (still outside any library code).
history=${GIT_RS_HISTORY:-$repo_root/crates/git-testkit/upstream-history.csv}
run_label=${GIT_RS_RUN_LABEL:-}
if [ -z "$run_label" ]; then
    run_label=$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date 2>/dev/null || printf 'unknown')
fi

# The hash algorithm test-lib.sh assumes. Upstream's test_oid database is keyed
# by hash algo; if neither GIT_TEST_DEFAULT_HASH nor GIT_TEST_BUILTIN_HASH is
# set, test-lib leaves $test_hash_algo empty and EVERY `test_oid` lookup aborts
# the script with "BUG: undefined key '...'", poisoning otherwise-passing
# assertions. A built checkout's GIT-BUILD-OPTIONS often omits
# GIT_TEST_BUILTIN_HASH, so we default it here to keep results meaningful.
# Callers can override (e.g. GIT_RS_DEFAULT_HASH=sha256).
default_hash=${GIT_RS_DEFAULT_HASH:-${GIT_TEST_DEFAULT_HASH:-sha1}}

# Map a script basename back to a friendly command name (inverse of the
# command_alias table) for the summary/history; falls back to the basename.
command_for_script() {
    case $1 in
        t0001-init.sh)             printf 'init\n' ;;
        t1006-cat-file.sh)         printf 'cat-file\n' ;;
        t1007-hash-object.sh)      printf 'hash-object\n' ;;
        t1300-config.sh)           printf 'config\n' ;;
        t1500-rev-parse.sh)        printf 'rev-parse\n' ;;
        t3000-ls-files-others.sh)  printf 'ls-files\n' ;;
        t3103-ls-tree-misc.sh)     printf 'ls-tree\n' ;;
        t1401-symbolic-ref.sh)     printf 'symbolic-ref\n' ;;
        *)                         printf '%s\n' "$1" ;;
    esac
}

# --- Run ------------------------------------------------------------------
{
    printf 'git-rs upstream test report\n'
    printf 'run label: %s\n' "$run_label"
    printf 'git-rs binary: %s\n' "$git_rs_bin"
    printf 'upstream t/: %s\n' "$upstream_t"
    printf 'default hash: %s\n' "$default_hash"
    printf 'per-script timeout: %ss\n' "$timeout_secs"
    printf '\n'
    printf '%-28s %-8s %5s %5s  %s\n' "SCRIPT" "RESULT" "OK" "FAIL" "DETAIL"
    printf '%s\n' "-------------------------------------------------------------------------"
} > "$report"

# Initialise the machine-readable per-command summary (overwritten each run).
printf 'script,command,result,ok,notok,total,plan_total\n' > "$summary"

# Ensure the append-only history has a header the first time it is created.
if [ ! -f "$history" ]; then
    printf 'label,script,command,result,ok,notok,total\n' > "$history"
fi

total=0
passed=0
failed=0
errored=0

run_one() {
    script=$1
    workdir=$(mktemp -d "${TMPDIR:-/tmp}/git-rs-upstream-run.XXXXXX")
    out_file="$workdir/output.txt"

    # Run the script from inside upstream_t so it can source test-lib.sh, with
    # GIT_TEST_INSTALLED pointed at our shim bindir. --no-bin-wrappers because an
    # installed-git layout has none; --root keeps trash dirs in our temp area.
    # GIT_TEST_DEFAULT_HASH primes test-lib's test_oid database (see note above).
    (
        cd "$upstream_t" || exit 99
        # Export explicitly (rather than a VAR=val prefix) so these reach the
        # grandchild `sh` regardless of how the chosen shell scopes assignment
        # prefixes on a shell-function invocation.
        export GIT_TEST_INSTALLED="$bindir"
        export GIT_RS_BIN="$git_rs_bin"
        export GIT_TEST_DEFAULT_HASH="$default_hash"
        run_with_timeout sh "$upstream_t/$script" \
            --no-bin-wrappers \
            --root="$workdir" \
            $extra_opts
    ) > "$out_file" 2>&1
    rc=$?

    # Parse TAP "ok"/"not ok" counts from the captured output.
    ok_count=$(grep -cE '^ok [0-9]' "$out_file" 2>/dev/null || printf '0')
    notok_count=$(grep -cE '^not ok [0-9]' "$out_file" 2>/dev/null || printf '0')
    plan_line=$(grep -E '^1\.\.[0-9]+' "$out_file" 2>/dev/null | head -n 1)
    plan_total=$(printf '%s' "$plan_line" | sed -n 's/^1\.\.\([0-9][0-9]*\).*/\1/p')
    last_lines=$(tail -n 3 "$out_file" 2>/dev/null | tr '\n' '|' | sed 's/|$//')
    command_name=$(command_for_script "$script")
    run_total=$((ok_count + notok_count))

    result="FAIL"
    detail=""
    # GNU timeout exits 124; our Perl alarm fallback exits 142 (128 + SIGALRM).
    if [ "$rc" -eq 124 ] || [ "$rc" -eq 142 ]; then
        result="TIMEOUT"
        errored=$((errored + 1))
        detail="exceeded ${timeout_secs}s (rc=$rc); ok=$ok_count notok=$notok_count so far"
    elif [ "$rc" -eq 0 ]; then
        result="PASS"
        passed=$((passed + 1))
        detail="$plan_line"
    else
        failed=$((failed + 1))
        detail="rc=$rc ${plan_line:+($plan_line) }${last_lines}"
    fi
    total=$((total + 1))

    printf '%-28s %-8s %5s %5s  %s\n' \
        "$script" "$result" "$ok_count" "$notok_count" "$detail" | tee -a "$report"

    # Machine-readable summary + append-only history rows.
    printf '%s,%s,%s,%s,%s,%s,%s\n' \
        "$script" "$command_name" "$result" "$ok_count" "$notok_count" \
        "$run_total" "${plan_total:-}" >> "$summary"
    printf '%s,%s,%s,%s,%s,%s,%s\n' \
        "$run_label" "$script" "$command_name" "$result" "$ok_count" \
        "$notok_count" "$run_total" >> "$history"

    # On anything but a clean pass, append the concrete failing TAP assertion
    # titles (the text after "not ok N - ...") plus a short tail. These titles
    # are the actionable gap map: each names a specific upstream behaviour
    # git-rs does not yet match.
    if [ "$result" != "PASS" ]; then
        {
            printf '\n----- %s (%s): failing assertions -----\n' "$script" "$result"
            # Strip the "# TODO known breakage" suffix so titles read cleanly;
            # those are upstream-expected failures, not git-rs regressions, but
            # we still list them (prefixed) for completeness.
            grep -E '^not ok [0-9]+ - ' "$out_file" 2>/dev/null \
                | sed -E 's/^not ok ([0-9]+) - /  [#\1] /' \
                || printf '  (no parseable "not ok" lines; script may have aborted early)\n'
            printf -- '----- %s (%s) last 25 lines -----\n' "$script" "$result"
            tail -n 25 "$out_file" 2>/dev/null
            printf -- '----- end %s -----\n\n' "$script"
        } >> "$report"
    fi

    rm -rf "$workdir"
}

log ""
log "Running upstream scripts against git-rs..."
log ""
printf '%-28s %-8s %5s %5s  %s\n' "SCRIPT" "RESULT" "OK" "FAIL" "DETAIL"
printf '%s\n' "-------------------------------------------------------------------------"

# Dedupe while preserving order, then run.
seen=""
for script in $scripts; do
    case " $seen " in *" $script "*) continue ;; esac
    seen="$seen $script"
    run_one "$script"
done

{
    printf '\n'
    printf 'PER-COMMAND PASS RATE (assertions):\n'
    printf '%-14s %-12s %6s %6s %6s  %s\n' \
        "COMMAND" "RESULT" "OK" "FAIL" "TOTAL" "PASS%"
    # Re-read the per-script summary CSV (skip its header) to print a tidy
    # per-command assertion pass-rate table.
    tail -n +2 "$summary" | while IFS=, read -r s_script s_cmd s_result s_ok s_notok s_total s_plan; do
        if [ "${s_total:-0}" -gt 0 ] 2>/dev/null; then
            pct=$(( s_ok * 100 / s_total ))
        else
            pct=0
        fi
        printf '%-14s %-12s %6s %6s %6s  %s%%\n' \
            "$s_cmd" "$s_result" "$s_ok" "$s_notok" "$s_total" "$pct"
    done
    printf '\n'
    printf 'SUMMARY: %s script(s): %s passed, %s failed, %s timed out.\n' \
        "$total" "$passed" "$failed" "$errored"
} | tee -a "$report"

log ""
log "Full report written to: $report"
log "Machine-readable summary: $summary"
log "Pass-rate history (appended): $history"

# Non-zero exit if anything did not pass, so CI/wrappers can gate on it.
if [ "$passed" -eq "$total" ]; then
    exit 0
fi
exit 1
