#!/bin/sh
# Run a single upstream t-file through the sley shim, keep full output.
# Usage: .run1.sh tNNNN-name.sh [extra test-lib opts]
set -e
WT=/home/heddleco/dev/HeddleCo/workspace/parity-rebase-i-cont
TGT=/home/heddleco/dev/HeddleCo/.heddleco-orchestrator/target/parity-rebase-i-cont
SLEY_BIN=${SLEY_BIN:-$TGT/release/sley}
UPSTREAM_T=/tmp/git-pcre-src/t
SCRIPT=$1; shift || true

BINDIR=$(mktemp -d /tmp/sley-run1-bindir.XXXXXX)
cat > "$BINDIR/git" <<SHIM
#!/bin/sh
SLEY_BIN='$SLEY_BIN'
SHIM_DIR='$BINDIR'
case "\${1:-}" in
  --exec-path) printf '%s\n' "\$SHIM_DIR"; exit 0;;
  --man-path|--html-path|--info-path) printf '%s\n' "\$SHIM_DIR"; exit 0;;
esac
exec "\$SLEY_BIN" "\$@"
SHIM
chmod +x "$BINDIR/git"

WORKDIR=$(mktemp -d /tmp/sley-run1-work.XXXXXX)
OUT=${OUTFILE:-/tmp/run1-out.txt}

export GIT_TEST_INSTALLED="$BINDIR"
export SLEY_BIN GIT_RS_BIN="$SLEY_BIN"
export GIT_TEST_DEFAULT_HASH=sha1
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=A GIT_AUTHOR_EMAIL=a@example.com
export GIT_COMMITTER_NAME=C GIT_COMMITTER_EMAIL=c@example.com
cd "$UPSTREAM_T"
sh "$UPSTREAM_T/$SCRIPT" --no-bin-wrappers --root="$WORKDIR" "$@" > "$OUT" 2>&1 || true
rm -rf "$BINDIR" "$WORKDIR"
echo "=== $SCRIPT ==="
grep -E '^(ok|not ok) [0-9]' "$OUT" | sed -E 's/^(not ok|ok) ([0-9]+).*/\1 \2/' | awk '{print $1}' | sort | uniq -c
echo "--- not ok ---"
grep -E '^not ok [0-9]' "$OUT" | sed -E 's/ #.*//'
echo "(full output: $OUT)"
