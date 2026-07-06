#!/usr/bin/env bash
# Extract the SLEY_TESTS list from .github/workflows/upstream-parity.yml.
# Prints one t-file per line (comments and YAML folding stripped).
#
# Usage:
#   scripts/extract-sley-tests-from-workflow.sh
#   scripts/extract-sley-tests-from-workflow.sh | wc -l
#   SLEY_TESTS="$(scripts/extract-sley-tests-from-workflow.sh | tr '\n' ' ')"
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="${ROOT}/.github/workflows/upstream-parity.yml"

if [[ ! -f "${WORKFLOW}" ]]; then
  echo "workflow not found: ${WORKFLOW}" >&2
  exit 1
fi

awk '
  /^          SLEY_TESTS: >-$/ { in_block = 1; next }
  in_block && /^          [A-Z_]+:/ { exit }
  in_block && /^            t[0-9]/ {
    line = $0
    sub(/^            /, "", line)
    sub(/#.*$/, "", line)
    gsub(/[[:space:]]+$/, "", line)
    if (line != "") print line
  }
' "${WORKFLOW}"