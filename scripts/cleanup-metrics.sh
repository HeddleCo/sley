#!/usr/bin/env bash
# Weekly cleanup dashboard (W40). Run from repo root.
set -euo pipefail

echo "=== CLI spine size ==="
wc -l crates/sley-cli/src/lib.rs crates/sley-cli/src/dispatch.rs 2>/dev/null || true
find crates/sley-cli/src/commands/remote -name '*.rs' -exec wc -l {} + 2>/dev/null | tail -1 || true

echo
echo "=== sley-cli facade deps ==="
rg '^sley-' crates/sley-cli/Cargo.toml || true

echo
echo "=== Hand-rolled option parsers ==="
rg -c 'fn parse_.*_options' crates/sley-cli || true

echo
echo "=== Global discovery sites ==="
rg -c 'discover_git_dir|GLOBAL_GIT_DIR' crates/sley-cli || true

echo
echo "=== Engine parity tests ==="
rg -c '^#\[test\]' crates/sley/tests/parity/ 2>/dev/null | awk -F: '{s+=$2} END {print s+0}'

echo
echo "=== Largest crate lib.rs files ==="
find crates -name lib.rs -exec wc -l {} + | sort -n | tail -5