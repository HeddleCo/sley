#!/usr/bin/env bash
# Backward-compatible entry point for listing the curated upstream test surface.
#
# The source of truth is now crates/sley-testkit/upstream-manifest.tsv; the
# workflow intentionally says only `SLEY_TESTS: curated`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNNER="${ROOT}/crates/sley-testkit/scripts/run-upstream-tests.sh"

if [[ -z "${SLEY_UPSTREAM_T:-}" && -z "${GIT_SRC_DIR:-}" ]]; then
  if [[ -f /tmp/git-src/t/test-lib.sh ]]; then
    export GIT_SRC_DIR=/tmp/git-src
  else
    echo "set GIT_SRC_DIR or SLEY_UPSTREAM_T to a Git v2.55.0 source checkout" >&2
    exit 1
  fi
fi

exec sh "${RUNNER}" --list-curated
