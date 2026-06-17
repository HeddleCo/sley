#!/bin/sh
# Usage: dbg.sh tNNNN-foo.sh [test-lib args like -v -x -i --run=14]
set -e
export GIT_SRC_DIR=/tmp/git-pcre-src
export PATH=/tmp/git-pcre-prefix/bin:$PATH
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_TEST_DEFAULT_HASH=sha1
export GIT_AUTHOR_NAME=A GIT_AUTHOR_EMAIL=a@example.com GIT_COMMITTER_NAME=C GIT_COMMITTER_EMAIL=c@example.com
SLEY_BIN=${SLEY_BIN:-/home/heddleco/dev/HeddleCo/.heddleco-orchestrator/target/parity-merge-porcelain/release/sley}
BINDIR=/home/heddleco/dev/HeddleCo/workspace/parity-merge-porcelain/.scratch/bindir
mkdir -p "$BINDIR"
cat > "$BINDIR/git" <<SHIM
#!/bin/sh
SLEY_BIN='$SLEY_BIN'
SHIM_DIR='$BINDIR'
case "\${1:-}" in
  --exec-path|--man-path|--html-path|--info-path) printf '%s\n' "\$SHIM_DIR"; exit 0;;
esac
exec "\$SLEY_BIN" "\$@"
SHIM
chmod +x "$BINDIR/git"
script="$1"; shift
export GIT_TEST_INSTALLED="$BINDIR"
cd "$GIT_SRC_DIR/t"
exec sh "$script" "$@"
