#!/bin/sh
# Run a single upstream t-file against the sley binary, verbose.
# Usage: scratch/run-one.sh t4211-line-log.sh [extra test-lib args like '-v' or '--run=5,6']
set -e
SLEY_BIN=/home/heddleco/dev/HeddleCo/.heddleco-orchestrator/target/parity-diff-log-3/release/sley
GIT_SRC_DIR=/tmp/git-pcre-src
upstream_t=$GIT_SRC_DIR/t
script=$1; shift || true

bindir=$(mktemp -d /tmp/sley-one-bindir.XXXXXX)
trap 'rm -rf "$bindir"' EXIT INT TERM
cat > "$bindir/git" <<SHIM
#!/bin/sh
SLEY_BIN='$SLEY_BIN'
case "\${1:-}" in
  --exec-path|--man-path|--html-path|--info-path) printf '%s\n' '$bindir'; exit 0;;
esac
exec "\$SLEY_BIN" "\$@"
SHIM
chmod +x "$bindir/git"

export GIT_TEST_INSTALLED="$bindir"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_TEST_DEFAULT_HASH=sha1
export GIT_AUTHOR_NAME=A GIT_AUTHOR_EMAIL=a@example.com GIT_COMMITTER_NAME=C GIT_COMMITTER_EMAIL=c@example.com
cd "$upstream_t"
sh "$script" "$@"
