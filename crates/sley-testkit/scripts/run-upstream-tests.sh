#!/bin/sh
#
# run-upstream-tests.sh — run UPSTREAM git's own t/*.sh test suite against the
# sley binary, as the ultimate parity oracle.
#
# Upstream git ships a TAP-emitting shell test framework (t/test-lib.sh plus
# t/tNNNN-*.sh scripts). test-lib.sh can exercise an *externally installed* git
# via the GIT_TEST_INSTALLED environment variable, which must point at a
# directory ("bindir") containing a working `git` executable. This script
# builds such a bindir whose `git` launches the Sley binary directly, then
# runs a configurable subset of the upstream scripts against it and aggregates
# the results.
#
# ---------------------------------------------------------------------------
# USAGE
#
#   crates/sley-testkit/scripts/run-upstream-tests.sh [SCRIPT...]
#
# Required: point the runner at an upstream git source checkout's t/ directory
# using ONE of:
#
#   SLEY_UPSTREAM_T     absolute path to the upstream git "t/" directory
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
# Then, from the repo root:
#
#       crates/sley-testkit/scripts/run-upstream-tests.sh
#
# Optional environment variables:
#
#   SLEY_BIN            absolute path to the sley binary. If unset we try
#                       $CARGO_BIN_EXE_sley, then target/debug/sley, and
#                       finally `cargo build -p sley-cli --bin sley`.
#   SLEY_TESTS          space-separated script list (overrides the default
#                       manifest-defined 891-script surface). The special
#                       values are `curated` and `foundational`. Positional
#                       args override this.
#   SLEY_UPSTREAM_MANIFEST
#                       ordered TSV manifest (default: ../upstream-manifest.tsv)
#                       defining inclusion, exclusions, applicability,
#                       prerequisites, and timing eligibility.
#   SLEY_TEST_TARGET    `sley` (default) or `oracle`. Oracle mode runs the same
#                       harness against SLEY_ORACLE_BIN/SLEY_TEST_GIT and does
#                       not build or resolve the Sley binary.
#   SLEY_ORACLE_BIN     installed, version-matched Git executable for oracle
#                       runs. Prefer a complete installed prefix, not a lone
#                       binary without its exec-path helpers.
#   SLEY_TEST_TIMEOUT   per-script timeout in seconds (default 120). 0 disables.
#                       Falls back to a Perl alarm(2) wrapper when neither
#                       timeout(1) nor gtimeout(1) is on PATH, so a hanging
#                       command (e.g. `rev-parse --short=N`) cannot stall the
#                       whole batch.
#   SLEY_REPORT         path for the human-readable report file
#                       (default: crates/sley-testkit/upstream-report.txt).
#   SLEY_SUMMARY        path for the machine-readable per-command CSV summary
#                       (default: <report>-summary.csv). Columns:
#                       script,command,result,ok,notok,total,plan_total.
#   SLEY_HISTORY        append-only per-command pass-rate history CSV
#                       (default: crates/sley-testkit/upstream-history.csv).
#                       Columns: label,script,command,result,ok,notok,total.
#   SLEY_TIMINGS        per-run script timing CSV (default:
#                       <summary-base>-timings.csv). Columns:
#                       label,script,command,result,elapsed_ms,ok,notok,total,plan_total.
#   SLEY_RUN_LABEL      label recorded in the report/history for this run (e.g.
#                       a git short-SHA or tag). Defaults to a UTC timestamp.
#                       The library never reads a clock; pass this to make runs
#                       reproducibly labelled.
#   SLEY_CELLS          exact per-cell CSV. Columns: target,script,cell,status,
#                       raw_result,directive,description. Status is one of
#                       PASS, FAIL, TODO, or SKIP.
#   SLEY_DETAILS        per-script classification CSV, including PASS, FAIL,
#                       ABORT, and TIMEOUT plus exact cell-status counts.
#   SLEY_ORACLE_CELLS   when set for a Sley run, compare its cells with this
#                       oracle cells CSV and write SLEY_COMPARISON.
#   SLEY_ORACLE_DETAILS optional oracle details CSV used to distinguish an
#                       incomplete oracle script in the comparison summary.
#   SLEY_COMPARISON     oracle/Sley per-cell comparison CSV (default:
#                       <summary-base>-comparison.csv).
#   SLEY_DEFAULT_HASH   hash algorithm primed into test-lib's test_oid database
#                       (default: sha1; or sha256). Without it test-lib aborts
#                       scripts with "BUG: undefined key" and pollutes results.
#   SLEY_TEST_OPTS      extra options forwarded to each upstream script
#                       (e.g. "--verbose" or "-x"). --no-bin-wrappers is always
#                       supplied because GIT_TEST_INSTALLED has no bin-wrappers.
#   SLEY_KEEP_TRASH     when non-empty, preserve each temporary --root directory
#                       and record it in the report. Useful for byte-level
#                       parity debugging after a failing upstream script.
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

# --- Foundational subset --------------------------------------------------
#
# Chosen to target commands sley already implements (init, hash-object,
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

# --- Locate the repo root and this script ---------------------------------
script_path=$0
case $script_path in
    /*) ;;
    *) script_path=$(pwd)/$script_path ;;
esac
script_dir=$(CDPATH= cd -- "$(dirname -- "$script_path")" && pwd)
# scripts/ -> sley-testkit -> crates -> repo root
testkit_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
repo_root=$(CDPATH= cd -- "$testkit_dir/../.." && pwd)
manifest=${SLEY_UPSTREAM_MANIFEST:-$testkit_dir/upstream-manifest.tsv}

log() { printf '%s\n' "$*" >&2; }
die() { printf 'run-upstream-tests: %s\n' "$*" >&2; exit 1; }

[ -f "$manifest" ] || die "curated manifest missing: $manifest"

# Print the last manifest rule matching SCRIPT. Fields after the script are:
# action, platforms, hashes, performance, prerequisites, reason. Ordered rules
# make a broad include plus explicit exclusions compact and reviewable.
manifest_record() {
    manifest_script=$1
    manifest_match=""
    tab=$(printf '\t')
    while IFS="$tab" read -r action selector platforms hashes performance prerequisites reason; do
        case ${action:-} in ''|'#'*) continue ;; esac
        case $manifest_script in
            $selector)
                manifest_match="$action${tab}$platforms${tab}$hashes${tab}$performance${tab}$prerequisites${tab}$reason"
                ;;
        esac
    done < "$manifest"
    [ -n "$manifest_match" ] || return 1
    printf '%s\n' "$manifest_match"
}

# --- Resolve the upstream t/ directory ------------------------------------
upstream_t=""
if [ -n "${SLEY_UPSTREAM_T:-}" ]; then
    upstream_t=$SLEY_UPSTREAM_T
elif [ -n "${GIT_SRC_DIR:-}" ]; then
    upstream_t=$GIT_SRC_DIR/t
fi

if [ -z "$upstream_t" ]; then
    log "SKIP: no upstream git t/ directory configured."
    log ""
    log "Set SLEY_UPSTREAM_T to the upstream git 't/' directory, or set"
    log "GIT_SRC_DIR to a git source root (we then use \$GIT_SRC_DIR/t)."
    log ""
    log "To obtain a usable checkout:"
    log "    git clone --depth=1 https://github.com/git/git /tmp/git-src"
    log "    cd /tmp/git-src && make GIT-BUILD-OPTIONS"
    log "    export GIT_SRC_DIR=/tmp/git-src"
    log ""
    log "Then re-run this script from the repo root."
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

# A stale, untracked tNNNN script changes the candidate set without changing
# the pinned source revision. That can silently enroll a local test in broad
# manifest rules and makes both parity counts and timing results
# irreproducible. Tarball sources have no .git entry and remain supported.
upstream_source_root=$(dirname -- "$upstream_t")
if [ -e "$upstream_source_root/.git" ]; then
    command -v git >/dev/null 2>&1 \
        || die "cannot verify upstream test inventory: git is not available"
    untracked_test_scripts=$(git -C "$upstream_source_root" ls-files --others -- \
        't/t[0-9][0-9][0-9][0-9]-*.sh' 2>/dev/null) \
        || die "cannot inspect untracked files in upstream Git checkout: $upstream_source_root"
    if [ -n "$untracked_test_scripts" ]; then
        die "untracked upstream test script(s) would alter manifest selection in $upstream_source_root:
$untracked_test_scripts
remove or track these files before running the upstream harness"
    fi
fi

manifest_selected_tests() {
    # Resolve every candidate in one awk process. Calling manifest_record for
    # each of ~1,000 scripts re-read the manifest ~1,000 times, adding a large
    # fixed cost to every paired timing run and diluting the wall-clock signal.
    for candidate in "$upstream_t"/t[0-9][0-9][0-9][0-9]-*.sh; do
        [ -f "$candidate" ] || continue
        printf '%s\n' "${candidate##*/}"
    done | awk -F '\t' '
        function glob_regex(glob,    regex, i, char, close_offset, class) {
            regex = "^"
            for (i = 1; i <= length(glob); i++) {
                char = substr(glob, i, 1)
                if (char == "*") regex = regex ".*"
                else if (char == "?") regex = regex "."
                else if (char == "[") {
                    close_offset = index(substr(glob, i + 1), "]")
                    if (close_offset == 0) regex = regex "\\["
                    else {
                        class = substr(glob, i + 1, close_offset - 1)
                        if (substr(class, 1, 1) == "!")
                            class = "^" substr(class, 2)
                        regex = regex "[" class "]"
                        i += close_offset
                    }
                } else {
                    if (char ~ /[.\\+^$(){}|]/) regex = regex "\\"
                    regex = regex char
                }
            }
            return regex "$"
        }
        FNR == NR {
            if ($0 ~ /^# expected_included=/) {
                expected_included = $0
                sub(/^# expected_included=/, "", expected_included)
            } else if ($0 ~ /^# expected_excluded=/) {
                expected_excluded = $0
                sub(/^# expected_excluded=/, "", expected_excluded)
            } else if ($1 == "include" || $1 == "exclude") {
                rules++
                action[rules] = $1
                selector[rules] = glob_regex($2)
            }
            next
        }
        {
            selected = ""
            for (rule = 1; rule <= rules; rule++)
                if ($0 ~ selector[rule]) selected = action[rule]
            if (selected == "include") included[++included_count] = $0
            else if (selected == "exclude") excluded_count++
        }
        END {
            if (expected_included != "" && included_count != expected_included) {
                print "run-upstream-tests: manifest selected " included_count \
                    " scripts, expected " expected_included \
                    " (wrong upstream version or stale manifest)" > "/dev/stderr"
                exit 1
            }
            if (expected_excluded != "" && excluded_count != expected_excluded) {
                print "run-upstream-tests: manifest excluded " excluded_count \
                    " scripts, expected " expected_excluded \
                    " (wrong upstream version or stale manifest)" > "/dev/stderr"
                exit 1
            }
            for (output_index = 1; output_index <= included_count; output_index++)
                print included[output_index]
        }
    ' "$manifest" -
}

# CI can consume the manifest without building or invoking either target.
case ${1:-} in
    --list-curated)
        manifest_selected_tests || exit $?
        exit 0
        ;;
    --validate-manifest)
        manifest_selected_tests >/dev/null || exit $?
        log "manifest valid: $(sed -n 's/^# expected_included=//p' "$manifest" | head -n 1) included script(s)"
        exit 0
        ;;
esac

build_dir=$(dirname -- "$upstream_t")
# Expose the build directory only because upstream test-lib and its own test
# helpers require it. Sley helper materialization must ignore these variables.
export GIT_BUILD_DIR="$build_dir"
export GIT_SRC_DIR="${GIT_SRC_DIR:-$build_dir}"
[ -f "$build_dir/GIT-BUILD-OPTIONS" ] \
    || die "$build_dir/GIT-BUILD-OPTIONS is missing; configure the pinned oracle build first"
expected_oracle_tag=$(sed -n 's/^# oracle_git=//p' "$manifest" | head -n 1)
expected_oracle_version=${expected_oracle_tag#v}
actual_source_version=$(sed -n 's/^GIT_VERSION=//p' "$build_dir/GIT-VERSION-FILE" 2>/dev/null | head -n 1)
if [ -n "$expected_oracle_version" ] \
    && [ "$actual_source_version" != "$expected_oracle_version" ]; then
    die "upstream test source is ${actual_source_version:-unknown}, manifest requires $expected_oracle_version"
fi
oracle_make_options=$(sed -n 's/^# oracle_make_options=//p' "$manifest" | head -n 1)
case " $oracle_make_options " in
    *" USE_LIBPCRE2=1 "*)
        build_pcre2=$(sed -n "s/^USE_LIBPCRE2='\(.*\)'$/\1/p" "$build_dir/GIT-BUILD-OPTIONS" | head -n 1)
        [ "$build_pcre2" = "1" ] \
            || die "oracle feature profile mismatch: rebuild $build_dir with USE_LIBPCRE2=1"
        ;;
esac

fake_ssh="$build_dir/t/helper/test-fake-ssh"
if [ ! -x "$fake_ssh" ]; then
    log "WARNING: test helper missing: $fake_ssh"
    log "         Upstream SSH transport tests copy this to GIT_SSH; without it"
    log "         they hang on the real /usr/bin/ssh (e.g. t5601-clone @ 900s)."
    log "         Fix: cd $build_dir && make t/helper/test-fake-ssh"
fi

# --- Resolve the target binary --------------------------------------------
test_target=${SLEY_TEST_TARGET:-sley}
case $test_target in
    sley|oracle) ;;
    *) die "SLEY_TEST_TARGET must be 'sley' or 'oracle' (got: $test_target)" ;;
esac

sley_bin=""
oracle_bin=""
oracle_exec_path=""
if [ "$test_target" = "sley" ]; then
    cargo_bin_exe=$(printenv CARGO_BIN_EXE_sley 2>/dev/null || true)
    if [ -n "${SLEY_BIN:-}" ]; then
        sley_bin=$SLEY_BIN
    elif [ -n "$cargo_bin_exe" ]; then
        sley_bin=$cargo_bin_exe
    elif [ -x "$repo_root/target/release/sley" ]; then
        sley_bin=$repo_root/target/release/sley
    elif [ -x "$repo_root/target/debug/sley" ]; then
        sley_bin=$repo_root/target/debug/sley
    fi

    if [ -z "$sley_bin" ] || [ ! -x "$sley_bin" ]; then
        log "sley binary not found; building with 'cargo build -p sley-cli --release --bin sley --features git-compat-i18n'..."
        ( cd "$repo_root" && cargo build -p sley-cli --release --bin sley --features git-compat-i18n ) \
            || die "cargo build -p sley-cli --release --bin sley --features git-compat-i18n failed"
        sley_bin=$repo_root/target/release/sley
    fi
    [ -x "$sley_bin" ] || die "sley binary still not executable: $sley_bin"
    case $sley_bin in /*) ;; *) sley_bin=$(pwd)/$sley_bin ;; esac
    target_bin=$sley_bin
    log "sley binary: $sley_bin"
else
    if [ -n "${SLEY_ORACLE_BIN:-}" ]; then
        oracle_bin=$SLEY_ORACLE_BIN
    elif [ -n "${SLEY_TEST_GIT:-}" ]; then
        oracle_bin=$SLEY_TEST_GIT
    elif [ -x "$build_dir/git" ]; then
        oracle_bin=$build_dir/git
    else
        oracle_bin=$(command -v git 2>/dev/null || true)
    fi
    [ -n "$oracle_bin" ] && [ -x "$oracle_bin" ] \
        || die "oracle Git not executable; set SLEY_ORACLE_BIN to a complete v2.55.0 installation"
    case $oracle_bin in /*) ;; *) oracle_bin=$(pwd)/$oracle_bin ;; esac
    actual_oracle_version=$($oracle_bin version 2>/dev/null | awk '{ print $3; exit }')
    if [ -n "$expected_oracle_version" ] \
        && [ "$actual_oracle_version" != "$expected_oracle_version" ]; then
        die "oracle Git version is $actual_oracle_version, manifest requires $expected_oracle_version"
    fi
    target_bin=$oracle_bin
    oracle_exec_path=$($oracle_bin --exec-path 2>/dev/null || true)
    if [ -z "$oracle_exec_path" ] || [ ! -d "$oracle_exec_path" ]; then
        # A built source checkout may have been configured for a prefix that is
        # not present on this machine. Its top-level build directory is still a
        # complete exec path when the dashed helpers were built there.
        source_exec_path=$(dirname -- "$oracle_bin")
        if [ "$source_exec_path" = "$build_dir" ] && [ -d "$source_exec_path" ]; then
            oracle_exec_path=$source_exec_path
        else
            die "oracle Git has no usable exec path; use a complete installed prefix: $oracle_bin"
        fi
    fi
    # test-lib invokes dashed, shell, remote, and auxiliary helpers through the
    # installed prefix. Catch a partial source-tree oracle before it turns into
    # hundreds of false failures. Executable helpers may carry `.exe` on
    # Windows; sourced shell libraries intentionally need only be regular files.
    for helper in git-upload-pack git-receive-pack git-http-backend git-remote-http git-submodule; do
        if [ ! -x "$oracle_exec_path/$helper" ] && [ ! -x "$oracle_exec_path/$helper.exe" ]; then
            die "oracle helper missing: $helper (exec path: $oracle_exec_path)"
        fi
    done
    for helper in git-sh-i18n git-sh-setup; do
        if [ ! -f "$oracle_exec_path/$helper" ]; then
            die "oracle shell library missing: $helper (exec path: $oracle_exec_path)"
        fi
    done
    oracle_bindir=$(dirname -- "$oracle_bin")
    if [ ! -x "$oracle_bindir/scalar" ] && [ ! -x "$oracle_bindir/scalar.exe" ]; then
        die "oracle auxiliary command missing: scalar (bindir: $oracle_bindir)"
    fi
    if [ -e "$oracle_exec_path/git-gui--askyesno" ] \
        || [ -e "$oracle_exec_path/git-gui--askyesno.exe" ]; then
        die "oracle feature profile mismatch: rebuild v2.55.0 with NO_TCLTK=YesPlease"
    fi
    log "oracle git: $oracle_bin"
    log "oracle exec path: $oracle_exec_path"
fi

# --- Build/select the installed-git bindir --------------------------------
#
# test-lib.sh runs `$GIT_TEST_INSTALLED/git --exec-path` early and aborts if it
# fails. Delegate that probe to sley too: feature-enabled builds can return a
# Git-compatible helper dir (git-sh-i18n, git-sh-i18n--envsubst), while lean
# builds still return their binary directory. Each test process exports
# SLEY_BIN so Sley-owned shell adapters can call back into the original binary.
generated_bindir=""
if [ "$test_target" = "sley" ]; then
    bindir=$(mktemp -d "${TMPDIR:-/tmp}/sley-upstream-bindir.XXXXXX") \
        || die "could not create temp bindir"
    generated_bindir=$bindir
    # Expose Sley under Git's installed binary name without inserting a shell
    # process in front of every command. A shell shim adds one `/bin/sh` launch
    # per assertion command (hundreds in scripts such as t7004), which is a
    # material candidate-only timing tax. Prefer a hardlink, then a symlink,
    # and finally a one-time binary copy for cross-filesystem/Windows hosts.
    installed_git=$bindir/git
    case $sley_bin in *.exe) installed_git=$bindir/git.exe ;; esac
    if ln "$sley_bin" "$installed_git" 2>/dev/null; then
        : direct hardlink
    elif ln -s "$sley_bin" "$installed_git" 2>/dev/null; then
        : direct symlink
    elif cp "$sley_bin" "$installed_git"; then
        : direct copy
    else
        die "could not expose Sley as a direct installed git launcher"
    fi
    chmod +x "$installed_git"
else
    bindir=$(dirname -- "$oracle_bin")
fi
cleanup() { [ -z "$generated_bindir" ] || rm -rf "$generated_bindir"; }
trap cleanup EXIT INT TERM

# Sanity-check the selected installed layout exactly as test-lib will see it.
if [ "$test_target" = "oracle" ]; then
    if ! GIT_EXEC_PATH="$oracle_exec_path" "$bindir/git" --exec-path >/dev/null 2>&1; then
        die "oracle installed-git layout failed its --exec-path self-check"
    fi
else
    sley_exec_path=$("$bindir/git" --exec-path 2>/dev/null || true)
    [ -n "$sley_exec_path" ] && [ -d "$sley_exec_path" ] \
        || die "sley installed-git layout failed its --exec-path self-check"
    provenance="$sley_exec_path/.sley-helper-provenance"
    [ -f "$provenance" ] && [ ! -L "$provenance" ] \
        || die "sley exec path has no owned helper provenance: $sley_exec_path"
    grep -qx 'owner=sley' "$provenance" \
        || die "sley helper provenance has the wrong owner: $provenance"
    grep -qx 'crate=sley-i18n' "$provenance" \
        || die "sley helper provenance has the wrong crate: $provenance"
    for helper in git-sh-i18n git-sh-i18n--envsubst git-upload-pack git-receive-pack git-http-backend; do
        [ -f "$sley_exec_path/$helper" ] && [ ! -L "$sley_exec_path/$helper" ] \
            || die "Sley-owned helper missing or borrowed through a symlink: $helper"
    done
    for helper in git-http-fetch git-http-push git-remote-ftp git-remote-ftps git-remote-http git-remote-https git-upload-archive; do
        [ ! -e "$sley_exec_path/$helper" ] && [ ! -L "$sley_exec_path/$helper" ] \
            || die "unimplemented helper must not be borrowed: $helper"
    done
fi

# --- Select scripts -------------------------------------------------------
selection=$*
if [ -z "$selection" ]; then
    selection=${SLEY_TESTS:-curated}
fi

if [ "$selection" = "curated" ]; then
    selection=$(manifest_selected_tests) || exit $?
elif [ "$selection" = "foundational" ]; then
    selection=$DEFAULT_TESTS
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

# SSH/git transport scripts copy test-fake-ssh to GIT_SSH; without the helper
# they fall through to real ssh and hang (t5601-clone is the canonical example).
if [ ! -x "$fake_ssh" ]; then
    needs_fake_ssh=""
    for script in $scripts; do
        case $script in
            t55*|t56*) needs_fake_ssh=1; break ;;
        esac
    done
    if [ -n "$needs_fake_ssh" ]; then
        die "test-fake-ssh is required for selected transport scripts but missing: $fake_ssh (run: cd $build_dir && make t/helper/test-fake-ssh)"
    fi
fi

# --- Timeout helper -------------------------------------------------------
#
# A per-script wall-clock cap matters here: some sley commands may still hang
# (e.g. `rev-parse --short=N` for N >= the hash length spins forever), and
# without a cap a single hang stalls the whole batch. We prefer GNU
# timeout(1)/gtimeout(1) when present; otherwise we fall back to a small Perl
# `alarm` wrapper (perl is required by upstream test-lib.sh, so it is always
# available in a usable checkout). The fallback's exit status for a timeout is
# 142 (128 + SIGALRM=14); we normalise both 124 (GNU timeout) and 142 to
# "TIMEOUT" below.
timeout_secs=${SLEY_TEST_TIMEOUT:-120}
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
# alarm, kills that whole group. This matters because a sley command can spin
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

report=${SLEY_REPORT:-$repo_root/crates/sley-testkit/upstream-report.txt}
extra_opts=${SLEY_TEST_OPTS:-}

# Machine-readable per-command summary (one CSV row per script):
#   script,command,result,ok,notok,total,plan_total
# Default lives next to the human report.
summary=${SLEY_SUMMARY:-${report%.txt}-summary.csv}

# Append-only per-command pass-rate history, so trends are visible across runs.
# Each row: label,script,command,result,ok,notok,total. The label is supplied
# by the caller (SLEY_RUN_LABEL) so the library never has to call a clock;
# when unset we fall back to a UTC timestamp from date(1) at the shell layer
# (still outside any library code).
history=${SLEY_HISTORY:-$repo_root/crates/sley-testkit/upstream-history.csv}

# Per-run script timings. This stays separate from the pass/fail summary so
# floor checks can continue to consume the stable seven-column CSV shape.
timings=${SLEY_TIMINGS:-${summary%.csv}-timings.csv}
cells=${SLEY_CELLS:-${summary%.csv}-cells.csv}
details=${SLEY_DETAILS:-${summary%.csv}-details.csv}
comparison=${SLEY_COMPARISON:-${summary%.csv}-comparison.csv}
comparison_summary=${SLEY_COMPARISON_SUMMARY:-${comparison%.csv}-summary.csv}
run_label=${SLEY_RUN_LABEL:-}
if [ -z "$run_label" ]; then
    run_label=$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date 2>/dev/null || printf 'unknown')
fi

# A fresh artifact root is the normal certification layout. Create all parent
# directories before running a script so a missing output directory cannot
# waste the run and discard its exact-cell evidence.
for artifact in "$report" "$summary" "$history" "$timings" "$cells" "$details" "$comparison" "$comparison_summary"; do
    artifact_parent=$(dirname -- "$artifact")
    mkdir -p "$artifact_parent" || die "could not create artifact directory: $artifact_parent"
done

# The hash algorithm test-lib.sh assumes. Upstream's test_oid database is keyed
# by hash algo; if neither GIT_TEST_DEFAULT_HASH nor GIT_TEST_BUILTIN_HASH is
# set, test-lib leaves $test_hash_algo empty and EVERY `test_oid` lookup aborts
# the script with "BUG: undefined key '...'", poisoning otherwise-passing
# assertions. A built checkout's GIT-BUILD-OPTIONS often omits
# GIT_TEST_BUILTIN_HASH, so we default it here to keep results meaningful.
# Callers can override (e.g. SLEY_DEFAULT_HASH=sha256).
default_hash=${SLEY_DEFAULT_HASH:-${GIT_TEST_DEFAULT_HASH:-sha1}}

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
    printf '%s upstream test report\n' "$test_target"
    printf 'run label: %s\n' "$run_label"
    printf 'target: %s\n' "$test_target"
    printf 'target binary: %s\n' "$target_bin"
    printf 'upstream t/: %s\n' "$upstream_t"
    printf 'manifest: %s\n' "$manifest"
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

printf 'label,script,command,result,elapsed_ms,ok,notok,total,plan_total\n' > "$timings"
printf 'target,script,cell,status,raw_result,directive,description\n' > "$cells"
printf 'target,script,result,exit_code,pass,fail,todo,skip,total_cells,plan_total,abort,timeout,missing_cells,extra_cells\n' > "$details"

total=0
passed=0
failed=0
errored=0
aborted=0
skipped_scripts=0

now_millis() {
    if command -v perl >/dev/null 2>&1; then
        perl -MTime::HiRes=time -e 'printf "%.0f\n", time() * 1000'
    else
        seconds=$(date +%s 2>/dev/null || printf 0)
        printf '%s000\n' "$seconds"
    fi
}

# Stale Apache instances from a prior upstream run can keep serving an old
# document root on LIB_HTTPD_PORT (default 18081), which makes every smart-HTTP
# clone in t5601 return 404 even though the trash repo.git exists.
stop_stale_httpd() {
    # Smart-HTTP tests bind LIB_HTTPD_PORT (often 18080); upstream defaults to
    # 18081. Kill listeners on every port we might reuse so a prior run cannot
    # serve the wrong document root and flake clone cells #110–#115.
    ports=""
    for port in "${LIB_HTTPD_PORT:-}" 18080 18081; do
        [ -n "$port" ] || continue
        case " $ports " in
            *" $port "*) continue ;;
        esac
        ports="$ports $port"
        if command -v lsof >/dev/null 2>&1; then
            pids=$(lsof -nP -iTCP:"$port" -sTCP:LISTEN -t 2>/dev/null || true)
            if [ -n "$pids" ]; then
                kill $pids 2>/dev/null || true
            fi
        fi
    done
}

# Parse upstream's top-level TAP lines without conflating directives with raw
# `ok`/`not ok`. The TSV is private scratch data; the public cells artifact is
# fully quoted CSV so assertion titles may safely contain commas.
parse_tap_cells() {
    tap_input=$1
    tap_output=$2
    # Test diagnostics may intentionally contain arbitrary path/message bytes.
    # BSD awk evaluates every record against the regular expressions below and
    # aborts with `towc: multibyte conversion failure` under a UTF-8 locale when
    # it sees an invalid byte, even if that record is not TAP. Parse TAP as a
    # byte stream so diagnostics cannot truncate the exact cell vector.
    LC_ALL=C awk '
        function emit(raw, text,    cell, description, upper, todo_pos, skip_pos, directive, status, cut) {
            cell = text
            sub(/[[:space:]].*$/, "", cell)
            description = text
            sub(/^[0-9]+([[:space:]]+-[[:space:]]*)?/, "", description)
            upper = toupper(description)
            todo_pos = index(upper, "# TODO")
            skip_pos = index(upper, "# SKIP")
            directive = ""
            cut = 0
            if (skip_pos > 0 && (todo_pos == 0 || skip_pos < todo_pos)) {
                directive = "SKIP"
                cut = skip_pos
            } else if (todo_pos > 0) {
                directive = "TODO"
                cut = todo_pos
            }
            if (cut > 0)
                description = substr(description, 1, cut - 1)
            sub(/[[:space:]]+$/, "", description)
            if (directive == "SKIP")
                status = "SKIP"
            else if (directive == "TODO")
                status = "TODO"
            else if (raw == "ok")
                status = "PASS"
            else
                status = "FAIL"
            gsub(/[\t\r]/, " ", description)
            printf "%s\t%s\t%s\t%s\t%s\n", cell, status, raw, directive, description
        }
        /^ok [0-9]+([[:space:]]+-|[[:space:]]+#|$)/ {
            text = $0
            sub(/^ok[[:space:]]+/, "", text)
            emit("ok", text)
            next
        }
        /^not ok [0-9]+([[:space:]]+-|[[:space:]]+#|$)/ {
            text = $0
            sub(/^not ok[[:space:]]+/, "", text)
            emit("not_ok", text)
            next
        }
        /^1\.\.0([[:space:]]+#[[:space:]]*SKIP|[[:space:]]*$)/ {
            description = $0
            sub(/^1\.\.0[[:space:]]*#[[:space:]]*SKIP[[:space:]]*/, "", description)
            gsub(/[\t\r]/, " ", description)
            printf "plan\tSKIP\tplan\tSKIP\t%s\n", description
        }
    ' "$tap_input" > "$tap_output"
}

append_cells_csv() {
    tap_tsv=$1
    tap_script=$2
    awk -F '\t' -v target="$test_target" -v script="$tap_script" '
        function q(value) { gsub(/"/, "\"\"", value); return "\"" value "\"" }
        { print q(target) "," q(script) "," q($1) "," q($2) "," q($3) "," q($4) "," q($5) }
    ' "$tap_tsv" >> "$cells"
}

compare_with_oracle() {
    oracle_cells=$1
    oracle_details=${SLEY_ORACLE_DETAILS:-}
    [ -f "$oracle_cells" ] || die "oracle cells artifact missing: $oracle_cells"
    comparison_rows=$(mktemp "${TMPDIR:-/tmp}/sley-upstream-comparison.XXXXXX") \
        || die "could not create comparison scratch file"
    comparison_scripts=$(mktemp "${TMPDIR:-/tmp}/sley-upstream-comparison-scripts.XXXXXX") \
        || die "could not create comparison summary scratch file"
    comparison_selected=$(mktemp "${TMPDIR:-/tmp}/sley-upstream-comparison-selected.XXXXXX") \
        || die "could not create comparison selection scratch file"
    awk -F, 'NR > 1 { print $2 }' "$details" > "$comparison_selected"

    awk -F, -v rows="$comparison_rows" '
        function clean(value) {
            sub(/^"/, "", value)
            sub(/"$/, "", value)
            gsub(/""/, "\"", value)
            return value
        }
        function note(script, cell, oracle_status, sley_status, kind) {
            print script "," cell "," oracle_status "," sley_status "," kind >> rows
            oracle_count[script] += (oracle_status != "")
            sley_count[script] += (sley_status != "")
            if (kind != "MATCH_PASS" && kind != "ORACLE_SKIP" && kind != "ORACLE_TODO")
                mismatches[script]++
            if (kind == "UNEXPECTED_SLEY_SKIP")
                unexpected[script]++
            if (kind == "MISSING_SLEY_CELL")
                missing[script]++
            if (kind == "EXTRA_SLEY_CELL")
                extra[script]++
            if (oracle_status == "PASS" && sley_status != "PASS")
                correctness_fail[script]++
            seen_script[script] = 1
        }
        FILENAME == ARGV[1] {
            selected_script[clean($1)] = 1
            next
        }
        FILENAME == ARGV[2] {
            if (FNR == 1) next
            script = clean($2); cell = clean($3); key = script SUBSEP cell
            if (script in selected_script)
                oracle_status[key] = clean($4)
            next
        }
        FILENAME == ARGV[3] {
            if (FNR == 1) next
            script = clean($2); cell = clean($3); key = script SUBSEP cell
            sley_status[key] = clean($4)
        }
        END {
            for (key in oracle_status) {
                split(key, parts, SUBSEP)
                script = parts[1]; cell = parts[2]
                os = oracle_status[key]; ss = sley_status[key]
                if (ss == "") kind = "MISSING_SLEY_CELL"
                else if (os == "SKIP" && ss == "SKIP") kind = "ORACLE_SKIP"
                else if (os == "TODO" && ss == "TODO") kind = "ORACLE_TODO"
                else if (os == "SKIP" || os == "TODO") kind = "STATUS_MISMATCH"
                else if (ss == "SKIP") kind = "UNEXPECTED_SLEY_SKIP"
                else if (os == "PASS" && ss == "PASS") kind = "MATCH_PASS"
                else if (os == "PASS" && ss == "FAIL") kind = "SLEY_FAILURE"
                else kind = "STATUS_MISMATCH"
                note(script, cell, os, ss, kind)
            }
            for (key in sley_status) {
                if (key in oracle_status) continue
                split(key, parts, SUBSEP)
                note(parts[1], parts[2], "", sley_status[key], "EXTRA_SLEY_CELL")
            }
            for (script in seen_script) {
                correctness = correctness_fail[script] ? "FAIL" : "PASS"
                print script "," oracle_count[script] + 0 "," sley_count[script] + 0 "," \
                    mismatches[script] + 0 "," unexpected[script] + 0 "," \
                    missing[script] + 0 "," extra[script] + 0 "," correctness
            }
        }
    ' "$comparison_selected" "$oracle_cells" "$cells" > "$comparison_scripts"

    {
        printf 'script,cell,oracle_status,sley_status,comparison\n'
        sort -t, -k1,1 -k2,2n "$comparison_rows"
    } > "$comparison"

    printf 'script,oracle_result,sley_result,oracle_cells,sley_cells,cell_vector,correctness,unexpected_sley_skips,missing_sley_cells,extra_sley_cells,performance_eligible,performance_comparison\n' > "$comparison_summary"
    sort -t, -k1,1 "$comparison_scripts" | while IFS=, read -r c_script c_oracle_count c_sley_count c_mismatches c_unexpected c_missing c_extra c_correctness; do
        record=$(manifest_record "$c_script" || true)
        tab=$(printf '\t')
        old_ifs=$IFS
        IFS="$tab"
        set -- $record
        IFS=$old_ifs
        c_performance=${4:-ineligible}
        c_oracle_result="UNKNOWN"
        if [ -n "$oracle_details" ] && [ -f "$oracle_details" ]; then
            c_oracle_result=$(awk -F, -v script="$c_script" '$2 == script { print $3; exit }' "$oracle_details")
            c_oracle_result=${c_oracle_result:-UNKNOWN}
        fi
        c_sley_result=$(awk -F, -v script="$c_script" '$2 == script { print $3; exit }' "$details")
        c_sley_result=${c_sley_result:-UNKNOWN}
        if [ "$c_sley_result" = "SKIP" ] && [ "$c_oracle_result" != "SKIP" ]; then
            c_unexpected=$((c_unexpected + 1))
            c_correctness=FAIL
        fi
        if [ "$c_mismatches" -eq 0 ]; then
            c_vector=EXACT
        else
            c_vector=INCOMPARABLE
        fi
        if [ "$c_performance" = "eligible" ] \
            && [ "$c_vector" = "EXACT" ] \
            && [ "$c_oracle_result" = "PASS" ] \
            && [ "$c_sley_result" = "PASS" ]; then
            c_performance_comparison=COMPARABLE
        else
            c_performance_comparison=INCOMPARABLE
        fi
        printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
            "$c_script" "$c_oracle_result" "$c_sley_result" "$c_oracle_count" \
            "$c_sley_count" "$c_vector" "$c_correctness" "$c_unexpected" \
            "$c_missing" "$c_extra" "$c_performance" "$c_performance_comparison" \
            >> "$comparison_summary"
    done
    rm -f "$comparison_rows" "$comparison_scripts" "$comparison_selected"
    log "Oracle/Sley cell comparison: $comparison"
    log "Oracle/Sley comparison summary: $comparison_summary"
}

run_one() {
    script=$1
    stop_stale_httpd
    workdir=$(mktemp -d "${TMPDIR:-/tmp}/sley-upstream-run.XXXXXX")
    out_file="$workdir/output.txt"
    start_ms=$(now_millis)

    # Run the script from inside upstream_t so it can source test-lib.sh, with
    # GIT_TEST_INSTALLED pointed at our direct-launch bindir. --no-bin-wrappers
    # because an installed-git layout has none; --root keeps trash dirs in our
    # temp area.
    # GIT_TEST_DEFAULT_HASH primes test-lib's test_oid database (see note above).
    (
        cd "$upstream_t" || exit 99
        # Export explicitly (rather than a VAR=val prefix) so these reach the
        # grandchild `sh` regardless of how the chosen shell scopes assignment
        # prefixes on a shell-function invocation.
        export GIT_TEST_INSTALLED="$bindir"
        if [ "$test_target" = "sley" ]; then
            export SLEY_BIN="$sley_bin"
        else
            export GIT_EXEC_PATH="$oracle_exec_path"
        fi
        export GIT_BUILD_DIR="$build_dir"
        export GIT_SRC_DIR="${GIT_SRC_DIR:-$build_dir}"
        export GIT_TEST_DEFAULT_HASH="$default_hash"
        # Git's own t/Makefile runs external chainlint once for the whole
        # suite and disables the per-script invocation. Besides duplicating
        # that work, chainlint.pl probes macOS through sysctl; restricted test
        # environments can reject the probe and contaminate assertions that
        # compare nested test-framework stderr byte-for-byte (notably t0000).
        # Keep the runtime &&-chain checks enabled; this switch only disables
        # the redundant external Perl chainlint pass.
        export GIT_TEST_EXT_CHAIN_LINT=0
        # Daemon-capable scripts should fail loudly when the environment cannot
        # bind a loopback listener; otherwise upstream marks them SKIP and the
        # floor checker sees a misleading PASS with zero assertions.
        export GIT_TEST_GIT_DAEMON=true
        run_with_timeout sh "$upstream_t/$script" \
            --no-bin-wrappers \
            --root="$workdir" \
            $extra_opts
    ) > "$out_file" 2>&1
    rc=$?
    end_ms=$(now_millis)
    elapsed_ms=$((end_ms - start_ms))
    if [ "$elapsed_ms" -lt 0 ] 2>/dev/null; then
        elapsed_ms=0
    fi

    # Preserve raw counts in the legacy summary, and publish exact classified
    # cells separately. A TODO `not ok` is not a Sley failure; an `ok` SKIP is
    # not evidence that an oracle-applicable cell passed.
    tap_tsv="$workdir/cells.tsv"
    parse_tap_cells "$out_file" "$tap_tsv"
    append_cells_csv "$tap_tsv" "$script"
    ok_count=$(awk -F '\t' '$3 == "ok" { n++ } END { print n + 0 }' "$tap_tsv")
    notok_count=$(awk -F '\t' '$3 == "not_ok" { n++ } END { print n + 0 }' "$tap_tsv")
    pass_count=$(awk -F '\t' '$2 == "PASS" { n++ } END { print n + 0 }' "$tap_tsv")
    fail_count=$(awk -F '\t' '$2 == "FAIL" { n++ } END { print n + 0 }' "$tap_tsv")
    todo_count=$(awk -F '\t' '$2 == "TODO" { n++ } END { print n + 0 }' "$tap_tsv")
    skip_count=$(awk -F '\t' '$2 == "SKIP" { n++ } END { print n + 0 }' "$tap_tsv")
    plan_line=$(grep -E '^1\.\.[0-9]+' "$out_file" 2>/dev/null | tail -n 1)
    plan_total=$(printf '%s' "$plan_line" | sed -n 's/^1\.\.\([0-9][0-9]*\).*/\1/p')
    plan_skip=0
    if printf '%s\n' "$plan_line" | grep -Eq '^1\.\.0[[:space:]]+#[[:space:]]*SKIP'; then
        plan_skip=1
    fi
    last_lines=$(tail -n 3 "$out_file" 2>/dev/null | tr '\n' '|' | sed 's/|$//')
    command_name=$(command_for_script "$script")
    run_total=$((ok_count + notok_count))

    result="FAIL"
    detail=""
    is_abort=0
    is_timeout=0
    missing_cells=0
    extra_cells=0
    if [ -n "$plan_total" ]; then
        if [ "$run_total" -lt "$plan_total" ]; then
            missing_cells=$((plan_total - run_total))
        elif [ "$run_total" -gt "$plan_total" ]; then
            extra_cells=$((run_total - plan_total))
        fi
    fi
    # GNU timeout exits 124; our Perl alarm fallback exits 142 (128 + SIGALRM).
    if [ "$rc" -eq 124 ] || [ "$rc" -eq 142 ]; then
        result="TIMEOUT"
        errored=$((errored + 1))
        is_timeout=1
        detail="exceeded ${timeout_secs}s (rc=$rc); ok=$ok_count notok=$notok_count so far"
    elif [ -z "$plan_total" ] || [ "$run_total" -ne "$plan_total" ]; then
        result="ABORT"
        aborted=$((aborted + 1))
        is_abort=1
        detail="rc=$rc incomplete TAP plan=${plan_total:-missing} observed=$run_total ${last_lines}"
    elif [ "$rc" -eq 0 ] && [ "$plan_skip" -eq 1 ]; then
        result="SKIP"
        skipped_scripts=$((skipped_scripts + 1))
        detail="$plan_line"
    elif [ "$rc" -eq 0 ] && [ "$fail_count" -eq 0 ]; then
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
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$run_label" "$script" "$command_name" "$result" "$elapsed_ms" \
        "$ok_count" "$notok_count" "$run_total" "${plan_total:-}" >> "$timings"
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$test_target" "$script" "$result" "$rc" "$pass_count" "$fail_count" \
        "$todo_count" "$skip_count" "$run_total" "${plan_total:-}" "$is_abort" \
        "$is_timeout" "$missing_cells" "$extra_cells" >> "$details"

    if [ -n "${SLEY_KEEP_TRASH:-}" ]; then
        printf '  trash=%s\n' "$workdir" >> "$report"
    fi

    # On anything but a clean pass, append the concrete failing TAP assertion
    # titles (the text after "not ok N - ...") plus a short tail. These titles
    # are the actionable gap map: each names a specific upstream behaviour
    # sley does not yet match.
    if [ "$result" != "PASS" ]; then
        {
            printf '\n----- %s (%s): failing assertions -----\n' "$script" "$result"
            # Strip the "# TODO known breakage" suffix so titles read cleanly;
            # those are upstream-expected failures, not sley regressions, but
            # we still list them (prefixed) for completeness.
            grep -E '^not ok [0-9]+ - ' "$out_file" 2>/dev/null \
                | sed -E 's/^not ok ([0-9]+) - /  [#\1] /' \
                || printf '  (no parseable "not ok" lines; script may have aborted early)\n'
            printf -- '----- %s (%s) last 25 lines -----\n' "$script" "$result"
            tail -n 25 "$out_file" 2>/dev/null
            printf -- '----- end %s -----\n\n' "$script"
        } >> "$report"
    fi

    if [ -z "${SLEY_KEEP_TRASH:-}" ]; then
        rm -rf "$workdir"
    fi
}

log ""
log "Running upstream scripts against $test_target..."
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

if [ "$test_target" = "sley" ] && [ -n "${SLEY_ORACLE_CELLS:-}" ]; then
    compare_with_oracle "$SLEY_ORACLE_CELLS"
fi

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
    awk -F, '
        NR > 1 { pass += $5; fail += $6; todo += $7; skip += $8 }
        END { printf "TAP CELL SUMMARY: pass=%d fail=%d todo=%d skip=%d.\n", pass, fail, todo, skip }
    ' "$details"
    printf 'SUMMARY: %s script(s): %s passed, %s skipped, %s failed, %s aborted, %s timed out.\n' \
        "$total" "$passed" "$skipped_scripts" "$failed" "$aborted" "$errored"
} | tee -a "$report"

log ""
log "Full report written to: $report"
log "Machine-readable summary: $summary"
log "Pass-rate history (appended): $history"
log "Per-script timings: $timings"
log "Exact TAP cells: $cells"
log "Per-script classifications: $details"

# Non-zero exit if anything did not pass, so CI/wrappers can gate on it.
if [ $((passed + skipped_scripts)) -eq "$total" ]; then
    exit 0
fi
exit 1
