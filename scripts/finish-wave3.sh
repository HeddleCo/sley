#!/usr/bin/env bash
# finish-wave3.sh — test, commit, and measure upstream ring-A for wave 3 parity work.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> crate tests"
cargo test -p sley-formats -p sley-config -p sley-refs -p sley-rev -p sley-worktree

echo "==> cli integration tests"
cargo test -p sley-cli --test init --test symbolic_ref --test ls_files
cargo test -p sley-cli log_format_gs

if ! git diff --quiet || ! git diff --cached --quiet || [ -n "$(git status -s)" ]; then
  echo "==> committing wave 3 slices"

  git add \
    crates/sley-refs/src/lib.rs \
    crates/sley-rev/src/lib.rs \
    crates/sley-worktree/src/lib.rs \
    crates/sley-cli/src/lib.rs \
    crates/sley-cli/src/log_format.rs
  if ! git diff --cached --quiet; then
    git commit -m "$(cat <<'EOF'
fix(refs,rev,cli): symref chains, log -g %gs, and reflog walk

Follow symbolic ref chains through checkout and update-ref, emit reflog
subjects for log -g --format=%gs, and wire peeled resolution for
ORIG_HEAD and branch symref chains exercised by t1401.
EOF
)"
  fi

  git add crates/sley-formats/src/lib.rs crates/sley-config/src/lib.rs
  if ! git diff --cached --quiet; then
    git commit -m "$(cat <<'EOF'
feat(formats): RepositoryBootstrap init parity foundations

Add InitOptions and RepositoryBootstrap for templates, separate gitdir,
shared repository mode, object/ref format precedence, and reftable init.
EOF
)"
  fi

  git add crates/sley-cli/tests/init.rs crates/sley-cli/tests/symbolic_ref.rs
  if ! git diff --cached --quiet; then
    git commit -m "$(cat <<'EOF'
test(cli): init and symbolic-ref upstream parity tests

Add integration tests comparing sley vs system git for init bootstrap
options and symbolic-ref reflog, top-level targets, and symref chains.
EOF
)"
  fi

  git add crates/sley-cli/tests/ls_files.rs
  if ! git diff --cached --quiet; then
    git commit -m "$(cat <<'EOF'
test(cli): ls-files --others and --directory upstream parity tests

Extend ls_files integration tests for basic --others, nested .git
boundaries, and --directory pathspec rollup scenarios from t3000.
EOF
)"
  fi

  # Any remaining wave-3 files (e.g. partial worktree tweaks)
  if [ -n "$(git status -s)" ]; then
    git add -A
    git commit -m "fix(parity): wave 3 remaining upstream parity adjustments" || true
  fi
else
  echo "==> working tree clean, nothing to commit"
fi

if [ -d "${GIT_SRC_DIR:-/tmp/git-src}/t" ]; then
  echo "==> upstream ring-A harness"
  GIT_SRC_DIR="${GIT_SRC_DIR:-/tmp/git-src}" \
    SLEY_RUN_LABEL="wave3-$(git rev-parse --short HEAD)" \
    crates/sley-testkit/scripts/run-upstream-tests.sh
else
  echo "==> skip upstream harness (set GIT_SRC_DIR to a built git checkout)"
fi

echo "==> done"