#!/bin/sh
# usage: .run1.sh <tNNNN-...sh> <run-spec e.g. 35-40 or 38>
SCRIPT=$1; shift
export GIT_SRC_DIR=/tmp/git-pcre-src
export PATH=/tmp/git-pcre-prefix/bin:$PATH
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_TEST_DEFAULT_HASH=sha1
export GIT_AUTHOR_NAME=A GIT_AUTHOR_EMAIL=a@example.com GIT_COMMITTER_NAME=C GIT_COMMITTER_EMAIL=c@example.com
export GIT_TEST_INSTALLED=/home/heddleco/dev/HeddleCo/workspace/parity-rebase-i/.sley-shim-bin
export SLEY_BIN=/home/heddleco/dev/HeddleCo/.heddleco-orchestrator/target/parity-rebase-i/release/sley
WD=$(mktemp -d /tmp/sley-run1.XXXXXX)
cd /tmp/git-pcre-src/t || exit 99
RUN="$1"
sh "$SCRIPT" --no-bin-wrappers --root="$WD" -v ${RUN:+--run=$RUN} 2>&1
rm -rf "$WD"
