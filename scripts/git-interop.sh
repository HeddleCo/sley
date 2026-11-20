#!/usr/bin/env bash
# Pair stock git with sley. Sley-only test runs miss these bugs because both
# ends of the connection are sley.
#
#   SLEY=target/debug/sley GIT=/opt/git-2.56.0/bin/git scripts/git-interop.sh
#
# Covers stock-client clone/fetch/push against a sley server over file://,
# git://, ssh (local stand-in), and smart HTTP, plus the format checks stock
# git runs on sley-written data: fsck --strict, commit-graph verify (single
# file, split chain, and a merged layer), multi-pack-index verify, and
# verify-pack.

set -euo pipefail

: "${SLEY:?set SLEY to the sley binary}"
: "${GIT:?set GIT to a stock git binary}"

ROOT=$(mktemp -d)
DAEMON_PID=
HTTP_PID=
cleanup() {
    local status=$?
    if [[ -n "${DAEMON_PID}" ]]; then
        kill "${DAEMON_PID}" 2>/dev/null || true
        wait "${DAEMON_PID}" 2>/dev/null || true
    fi
    if [[ -n "${HTTP_PID}" ]]; then
        kill "${HTTP_PID}" 2>/dev/null || true
        wait "${HTTP_PID}" 2>/dev/null || true
    fi
    rm -rf "${ROOT}"
    exit "${status}"
}
trap cleanup EXIT

say() {
    printf '== %s\n' "$*"
}

touch "${ROOT}/empty-config"
export GIT_AUTHOR_NAME='Interop User'
export GIT_AUTHOR_EMAIL='interop@example.invalid'
export GIT_COMMITTER_NAME='Interop User'
export GIT_COMMITTER_EMAIL='interop@example.invalid'
export GIT_CONFIG_GLOBAL="${ROOT}/empty-config"
export GIT_CONFIG_SYSTEM="${ROOT}/empty-config"
unset GIT_CONFIG_COUNT || true

g() { "${GIT}" "$@"; }
s() { "${SLEY}" "$@"; }

free_port() {
    python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

wait_for_log() {
    local pid=$1
    local log=$2
    local needle=$3
    local _try
    for _try in $(seq 1 100); do
        if grep -q "${needle}" "${log}" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "${pid}" 2>/dev/null; then
            echo "process ${pid} exited before ${needle}" >&2
            cat "${log}" >&2 || true
            return 1
        fi
        sleep 0.05
    done
    echo "timed out waiting for ${needle}" >&2
    cat "${log}" >&2 || true
    return 1
}

wait_for_port() {
    local port=$1
    python3 - "${port}" <<'PY'
import socket, sys, time
port = int(sys.argv[1])
for _ in range(100):
    try:
        sock = socket.create_connection(("127.0.0.1", port), 0.2)
    except OSError:
        time.sleep(0.05)
        continue
    sock.close()
    sys.exit(0)
sys.exit(1)
PY
}

commit_file() {
    local repo=$1
    local name=$2
    local date=$3
    printf '%s\n' "${name}" >"${repo}/${name}"
    g -C "${repo}" add "${name}"
    GIT_AUTHOR_DATE="${date}" GIT_COMMITTER_DATE="${date}" \
        g -C "${repo}" -c commit.gpgsign=false commit -qm "${name}"
}

same_commit() {
    local left=$1
    local right=$2
    local label=$3
    local a b
    a=$(g -C "${left}" rev-parse HEAD)
    b=$(g -C "${right}" rev-parse HEAD)
    if [[ "${a}" != "${b}" ]]; then
        echo "${label}: ${a} != ${b}" >&2
        return 1
    fi
}

contains_matches() {
    local repo=$1
    local commit=$2
    diff -u \
        <(g -C "${repo}" -c core.commitGraph=true branch --contains "${commit}") \
        <(g -C "${repo}" -c core.commitGraph=false branch --contains "${commit}")
}

say "stock git $($GIT version)"
say "sley $($SLEY version)"

# --- server history -------------------------------------------------------
WT=${ROOT}/wt
SRV=${ROOT}/srv.git
s init -q -b main "${WT}"
EQUAL='1700000000 +0000'
for name in a b c; do
    printf '%s\n' "${name}" >"${WT}/${name}"
    s -C "${WT}" add "${name}"
    GIT_AUTHOR_DATE="${EQUAL}" GIT_COMMITTER_DATE="${EQUAL}" \
        s -C "${WT}" -c user.name='Interop User' -c user.email='interop@example.invalid' \
        -c commit.gpgsign=false commit -qm "${name}"
done
s -C "${WT}" branch topic HEAD~1
s clone -q --bare "${WT}" "${SRV}"
s -C "${SRV}" config http.receivepack true
s -C "${SRV}" config uploadpack.allowFilter true
g -C "${SRV}" tag -a v1 -m v1 HEAD

UPLOAD="${SLEY} upload-pack"
RECEIVE="${SLEY} receive-pack"

say "file:// clone, fetch, push"
FILE_CLONE=${ROOT}/file-clone
g clone -q --no-local -u "${UPLOAD}" "file://${SRV}" "${FILE_CLONE}"
same_commit "${FILE_CLONE}" "${WT}" "file clone"
g -C "${FILE_CLONE}" fsck --strict >/dev/null
printf 'd\n' >"${WT}/d"
s -C "${WT}" add d
GIT_AUTHOR_DATE="${EQUAL}" GIT_COMMITTER_DATE="${EQUAL}" \
    s -C "${WT}" -c user.name='Interop User' -c user.email='interop@example.invalid' \
    -c commit.gpgsign=false commit -qm d
s -C "${WT}" push -q "${SRV}" main
g -C "${FILE_CLONE}" fetch -q origin
g -C "${FILE_CLONE}" merge -q --ff-only origin/main
same_commit "${FILE_CLONE}" "${WT}" "file fetch"
printf 'pushed\n' >"${FILE_CLONE}/pushed"
g -C "${FILE_CLONE}" add pushed
g -C "${FILE_CLONE}" -c commit.gpgsign=false commit -qm pushed
g -C "${FILE_CLONE}" push -q --receive-pack="${RECEIVE}" origin HEAD:refs/heads/pushed
test "$(g -C "${SRV}" rev-parse pushed)" = "$(g -C "${FILE_CLONE}" rev-parse HEAD)"
printf 'pushed2\n' >>"${FILE_CLONE}/pushed"
g -C "${FILE_CLONE}" -c commit.gpgsign=false commit -qam pushed2
g -C "${FILE_CLONE}" push -q --receive-pack="${RECEIVE}" origin HEAD:refs/heads/pushed
test "$(g -C "${SRV}" rev-parse pushed)" = "$(g -C "${FILE_CLONE}" rev-parse HEAD)"
g -C "${FILE_CLONE}" push -q --receive-pack="${RECEIVE}" origin :refs/heads/pushed
if g -C "${SRV}" rev-parse -q --verify refs/heads/pushed >/dev/null; then
    echo "file:// delete left refs/heads/pushed" >&2
    exit 1
fi
g -C "${SRV}" fsck --strict >/dev/null

say "v0 ls-remote peeled tags and capabilities"
V0_SLEY=${ROOT}/v0-sley-ls
V0_STOCK=${ROOT}/v0-stock-ls
g -c protocol.version=0 ls-remote --upload-pack="${UPLOAD}" "file://${SRV}" >"${V0_SLEY}"
g -c protocol.version=0 ls-remote "file://${SRV}" >"${V0_STOCK}"
diff -u "${V0_STOCK}" "${V0_SLEY}"
grep -F -q 'refs/tags/v1^{}' "${V0_SLEY}"
V0_TRACE=${ROOT}/v0-caps.trace
g -c protocol.version=0 ls-remote --upload-pack="${UPLOAD}" "file://${SRV}" >/dev/null
GIT_TRACE_PACKET=${V0_TRACE} g -c protocol.version=0 ls-remote --upload-pack="${UPLOAD}" "file://${SRV}" >/dev/null
for token in shallow side-band-64k ofs-delta include-tag filter symref agent object-format; do
    if ! grep -q "${token}" "${V0_TRACE}"; then
        echo "v0 advertisement missing ${token}" >&2
        exit 1
    fi
done

say "v0 shallow and partial clone"
V0_SHALLOW=${ROOT}/v0-shallow
g -c protocol.version=0 clone -q --no-local --depth=1 -u "${UPLOAD}" "file://${SRV}" "${V0_SHALLOW}"
test "$(g -C "${V0_SHALLOW}" rev-list --count HEAD)" = 1
# `clone -u` applies only to the initial fetch. Checkout's promisor child
# looks up git-upload-pack on GIT_EXEC_PATH, so point that name at sley or
# the lazy blob fetch would hit stock upload-pack (allowReachable defaults off).
SLEY_EXEC=${ROOT}/sley-exec
mkdir -p "${SLEY_EXEC}"
ln -s "${GIT%/*}/../libexec/git-core/"* "${SLEY_EXEC}/"
rm -f "${SLEY_EXEC}/git-upload-pack"
cat >"${SLEY_EXEC}/git-upload-pack" <<EOF
#!/bin/sh
exec "${SLEY}" upload-pack "\$@"
EOF
chmod +x "${SLEY_EXEC}/git-upload-pack"
V0_PARTIAL=${ROOT}/v0-partial
GIT_EXEC_PATH="${SLEY_EXEC}" g -c protocol.version=0 clone -q --no-checkout --no-local --filter=blob:none -u "${UPLOAD}" "file://${SRV}" "${V0_PARTIAL}"
test "$(g -C "${V0_PARTIAL}" config remote.origin.promisor)" = true
test "$(g -C "${V0_PARTIAL}" config remote.origin.partialclonefilter)" = blob:none
# Confirm the filtered pack omitted blobs before checkout hydrates them.
GIT_NO_LAZY_FETCH=1 g -C "${V0_PARTIAL}" rev-list --objects --all --missing=print | grep -q '^?'
GIT_EXEC_PATH="${SLEY_EXEC}" g -C "${V0_PARTIAL}" checkout -q
test -f "${V0_PARTIAL}/a"

say "git:// clone and push"
DPORT=$(free_port)
DAEMON_LOG=${ROOT}/daemon.log
s daemon --listen=127.0.0.1 --port="${DPORT}" --export-all --enable=receive-pack \
    --reuseaddr --verbose "${ROOT}" >"${DAEMON_LOG}" 2>&1 &
DAEMON_PID=$!
wait_for_log "${DAEMON_PID}" "${DAEMON_LOG}" "Ready to rumble"
GIT_URL="git://127.0.0.1:${DPORT}/srv.git"
GIT_CLONE=${ROOT}/git-clone
g clone -q "${GIT_URL}" "${GIT_CLONE}"
same_commit "${GIT_CLONE}" "${WT}" "git:// clone"
printf 'via-git\n' >"${GIT_CLONE}/via-git"
g -C "${GIT_CLONE}" add via-git
g -C "${GIT_CLONE}" -c commit.gpgsign=false commit -qm via-git
g -C "${GIT_CLONE}" push -q origin HEAD:refs/heads/viagit
test "$(g -C "${SRV}" rev-parse viagit)" = "$(g -C "${GIT_CLONE}" rev-parse HEAD)"
g -C "${SRV}" fsck --strict >/dev/null
say "git:// v0 shallow and partial"
V0_GIT_SHALLOW=${ROOT}/v0-git-shallow
g -c protocol.version=0 clone -q --depth=1 "${GIT_URL}" "${V0_GIT_SHALLOW}"
test "$(g -C "${V0_GIT_SHALLOW}" rev-list --count HEAD)" = 1
V0_GIT_PARTIAL=${ROOT}/v0-git-partial
g -c protocol.version=0 clone -q --no-checkout --filter=blob:none "${GIT_URL}" "${V0_GIT_PARTIAL}"
GIT_NO_LAZY_FETCH=1 g -C "${V0_GIT_PARTIAL}" rev-list --objects --all --missing=print | grep -q '^?'
g -C "${V0_GIT_PARTIAL}" checkout -q
test -f "${V0_GIT_PARTIAL}/a"
kill "${DAEMON_PID}" 2>/dev/null || true
wait "${DAEMON_PID}" 2>/dev/null || true
DAEMON_PID=

say "ssh clone and push"
FAKE_SSH=${ROOT}/fake-ssh
cat >"${FAKE_SSH}" <<'EOF'
#!/bin/sh
last=
for arg in "$@"; do
    last=$arg
done
eval "$last"
EOF
chmod +x "${FAKE_SSH}"
SSH_CLONE=${ROOT}/ssh-clone
GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=simple \
    g clone -q -u "${UPLOAD}" "ssh://git@127.0.0.1${SRV}" "${SSH_CLONE}"
same_commit "${SSH_CLONE}" "${WT}" "ssh clone"
printf 'via-ssh\n' >"${SSH_CLONE}/via-ssh"
g -C "${SSH_CLONE}" add via-ssh
g -C "${SSH_CLONE}" -c commit.gpgsign=false commit -qm via-ssh
GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=simple \
    g -C "${SSH_CLONE}" push -q --receive-pack="${RECEIVE}" origin HEAD:refs/heads/viassh
test "$(g -C "${SRV}" rev-parse viassh)" = "$(g -C "${SSH_CLONE}" rev-parse HEAD)"
g -C "${SRV}" fsck --strict >/dev/null

say "smart HTTP clone and push"
HPORT=$(free_port)
HTTP_PY=${ROOT}/http_backend.py
cat >"${HTTP_PY}" <<'PY'
import os
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sley = sys.argv[1]
root = sys.argv[2]
port = int(sys.argv[3])

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self.serve()

    def do_POST(self):
        self.serve()

    def serve(self):
        path, _, query = self.path.partition("?")
        length = int(self.headers.get("Content-Length", "0") or "0")
        body = self.rfile.read(length) if length else b""
        env = os.environ.copy()
        env["REQUEST_METHOD"] = self.command
        env["PATH_INFO"] = path
        env["QUERY_STRING"] = query
        env["GIT_PROJECT_ROOT"] = root
        env["GIT_HTTP_EXPORT_ALL"] = "1"
        content_type = self.headers.get("Content-Type")
        if content_type:
            env["CONTENT_TYPE"] = content_type
        git_protocol = self.headers.get("Git-Protocol")
        if git_protocol:
            env["HTTP_GIT_PROTOCOL"] = git_protocol
        proc = subprocess.run(
            [sley, "http-backend"],
            input=body,
            capture_output=True,
            env=env,
            check=False,
        )
        raw = proc.stdout
        sep = b"\r\n\r\n"
        if sep in raw:
            header, body_out = raw.split(sep, 1)
            lines = header.split(b"\r\n")
        elif b"\n\n" in raw:
            header, body_out = raw.split(b"\n\n", 1)
            lines = header.split(b"\n")
        else:
            lines = []
            body_out = raw
        status = 200
        fields = []
        for line in lines:
            if not line:
                continue
            if line.lower().startswith(b"status:"):
                status = int(line.split()[1])
            elif b":" in line:
                fields.append(line)
        if proc.returncode != 0:
            sys.stderr.buffer.write(proc.stderr)
        self.send_response(status)
        for line in fields:
            key, value = line.split(b":", 1)
            self.send_header(key.decode(), value.decode().strip())
        self.send_header("Content-Length", str(len(body_out)))
        self.end_headers()
        self.wfile.write(body_out)

    def log_message(self, fmt, *args):
        return

ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
python3 "${HTTP_PY}" "${SLEY}" "${ROOT}" "${HPORT}" >"${ROOT}/http.log" 2>&1 &
HTTP_PID=$!
wait_for_port "${HPORT}"
HTTP_URL="http://127.0.0.1:${HPORT}/srv.git"
HTTP_CLONE=${ROOT}/http-clone
g clone -q "${HTTP_URL}" "${HTTP_CLONE}"
same_commit "${HTTP_CLONE}" "${WT}" "http clone"
printf 'via-http\n' >"${HTTP_CLONE}/via-http"
g -C "${HTTP_CLONE}" add via-http
g -C "${HTTP_CLONE}" -c commit.gpgsign=false commit -qm via-http
g -C "${HTTP_CLONE}" push -q origin HEAD:refs/heads/viahttp
test "$(g -C "${SRV}" rev-parse viahttp)" = "$(g -C "${HTTP_CLONE}" rev-parse HEAD)"
g -C "${SRV}" fsck --strict >/dev/null
say "smart HTTP v0 clone"
HTTP_V0=${ROOT}/http-v0
g -c protocol.version=0 clone -q "${HTTP_URL}" "${HTTP_V0}"
same_commit "${HTTP_V0}" "${WT}" "http v0 clone"
kill "${HTTP_PID}" 2>/dev/null || true
wait "${HTTP_PID}" 2>/dev/null || true
HTTP_PID=

# --- formats sley writes, stock git reads --------------------------------
say "sley-written formats"
FMT=${ROOT}/formats
g init -q -b main "${FMT}"
for name in c0 c1 c2; do
    commit_file "${FMT}" "${name}" "${EQUAL}"
done
commit_file "${FMT}" c3 '1700000005 +0000'
commit_file "${FMT}" c4 "${EQUAL}"
for name in c5 c6 c7; do
    commit_file "${FMT}" "${name}" "${EQUAL}"
done
EARLY=$(g -C "${FMT}" rev-parse HEAD~6)
g -C "${FMT}" branch topic HEAD~2
g -C "${FMT}" fsck --strict >/dev/null

s -C "${FMT}" commit-graph write --reachable
g -C "${FMT}" commit-graph verify
contains_matches "${FMT}" "${EARLY}"

for name in c8 c9; do
    commit_file "${FMT}" "${name}" "${EQUAL}"
done
s -C "${FMT}" commit-graph write --reachable --split=no-merge
test -f "${FMT}/.git/objects/info/commit-graphs/commit-graph-chain"
CHAIN_LINES=$(grep -c . "${FMT}/.git/objects/info/commit-graphs/commit-graph-chain")
test "${CHAIN_LINES}" = 2
g -C "${FMT}" commit-graph verify
contains_matches "${FMT}" "${EARLY}"

for name in c10 c11 c12; do
    commit_file "${FMT}" "${name}" "${EQUAL}"
done
s -C "${FMT}" commit-graph write --reachable --split --size-multiple=1
CHAIN_LINES=$(grep -c . "${FMT}/.git/objects/info/commit-graphs/commit-graph-chain")
test "${CHAIN_LINES}" = 2
g -C "${FMT}" commit-graph verify
contains_matches "${FMT}" "${EARLY}"

s -C "${FMT}" repack -adbq --write-midx
found_pack=0
for idx in "${FMT}/.git/objects/pack/"*.idx; do
    found_pack=1
    g verify-pack "${idx%.idx}.pack" >/dev/null
done
test "${found_pack}" = 1
g -C "${FMT}" multi-pack-index verify
g -C "${FMT}" commit-graph verify
g -C "${FMT}" fsck --strict >/dev/null

printf 'intent\n' >"${FMT}/ita"
s -C "${FMT}" add -N ita
g -C "${FMT}" fsck --strict >/dev/null
g -C "${FMT}" cat-file -e e69de29bb2d1d6434b8b29ae775ad8c2e48c5391

say "bundle-uri capability order and empty values"
s -C "${SRV}" config uploadpack.advertiseBundleURIs true
s -C "${SRV}" config bundle.version 1
s -C "${SRV}" config bundle.mode all
s -C "${SRV}" config bundle.x.uri ""
BUNDLE_TRACE=${ROOT}/bundle.trace
BUNDLE_CLONE=${ROOT}/bundle-clone
GIT_TRACE_PACKET=${BUNDLE_TRACE} g -c transfer.bundleURI=true -c protocol.version=2 \
    clone -q --no-local -u "${UPLOAD}" "file://${SRV}" "${BUNDLE_CLONE}"
if grep -q 'bundle.x.uri=$' "${BUNDLE_TRACE}"; then
    echo "server sent empty bundle.x.uri=" >&2
    exit 1
fi
same_commit "${BUNDLE_CLONE}" "${WT}" "bundle-uri clone"

say "promisor-remote without a value is rejected"
python3 - "${SLEY}" "${SRV}" <<'PY'
import subprocess, sys
sley, repo = sys.argv[1], sys.argv[2]
def pkt(payload: bytes) -> bytes:
    return f"{len(payload) + 4:04x}".encode() + payload
body = b"".join([
    pkt(b"agent=git/2.56.0\n"),
    pkt(b"command=bundle-uri\n"),
    pkt(b"promisor-remote\n"),
    b"0000",
])
proc = subprocess.run(
    [sley, "upload-pack", repo],
    input=body,
    capture_output=True,
    env={**dict(**{k: v for k, v in __import__("os").environ.items()}), "GIT_PROTOCOL": "version=2"},
)
text = proc.stderr.decode("utf-8", "replace") + proc.stdout.decode("utf-8", "replace")
if proc.returncode == 0 or "promisor-remote" not in text:
    sys.stderr.write(text)
    sys.exit("promisor-remote with no value was accepted\n")
PY

say "sley client against stock upload-pack"
# Change `a` so the previous blob is not in the tip. Checkout top-up fetches
# the tip blob; cat-file must still lazy-fetch the historical one.
printf 'a2\n' >"${WT}/a"
s -C "${WT}" add a
GIT_AUTHOR_DATE="${EQUAL}" GIT_COMMITTER_DATE="${EQUAL}" \
    s -C "${WT}" -c user.name='Interop User' -c user.email='interop@example.invalid' \
    -c commit.gpgsign=false commit -qm a2
STOCK=${ROOT}/stock.git
g clone -q --bare "${WT}" "${STOCK}"
g -C "${STOCK}" config uploadpack.allowFilter true
g -C "${STOCK}" config uploadpack.allowReachableSHA1InWant true
GU="$("${GIT}" --exec-path)/git-upload-pack"
SLEY_LS=${ROOT}/sley-ls
STOCK_LS=${ROOT}/stock-ls
s ls-remote --upload-pack="${GU}" "file://${STOCK}" >"${SLEY_LS}"
g ls-remote "file://${STOCK}" >"${STOCK_LS}"
diff -u "${STOCK_LS}" "${SLEY_LS}"

INVOKE_LOG=${ROOT}/clone-u-invoked
WRAPPER=${ROOT}/upload-pack-wrapper
cat >"${WRAPPER}" <<EOF
#!/bin/sh
echo invoked >> "${INVOKE_LOG}"
exec "${GU}" "\$@"
EOF
chmod +x "${WRAPPER}"
SLEY_FILE_CLONE=${ROOT}/sley-file-clone
s clone -q -u "${WRAPPER}" "file://${STOCK}" "${SLEY_FILE_CLONE}"
grep -q invoked "${INVOKE_LOG}"
same_commit "${SLEY_FILE_CLONE}" "${WT}" "sley clone -u stock upload-pack"

SLEY_SSH=${ROOT}/sley-ssh-partial
GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=simple \
    s clone -q --filter=blob:none -u "${GU}" "ssh://git@127.0.0.1${STOCK}" "${SLEY_SSH}"
test -f "${SLEY_SSH}/a"
OLD_BLOB=$(g -C "${STOCK}" rev-parse 'HEAD~1:a')
if GIT_NO_LAZY_FETCH=1 g -C "${SLEY_SSH}" cat-file -e "${OLD_BLOB}" >/dev/null 2>&1; then
    echo "ssh partial clone contained historical blob ${OLD_BLOB}" >&2
    exit 1
fi
# Later fetches use the remote's `git-upload-pack` (clone -u is clone-time
# only, matching stock git). Point that name at the stock binary, and keep
# the same SSH stand-in the clone used.
export PATH="$(dirname "${GU}"):${PATH}"
export GIT_SSH_COMMAND="${FAKE_SSH}"
export GIT_SSH_VARIANT=simple
s -C "${SLEY_SSH}" cat-file -s "${OLD_BLOB}" >/dev/null

SLEY_FILE_PARTIAL=${ROOT}/sley-file-partial
# In-process file:// promisor hydrate (stock smart HTTP is not running here).
s clone -q --no-checkout --filter=blob:none "file://${STOCK}" "${SLEY_FILE_PARTIAL}"
s -C "${SLEY_FILE_PARTIAL}" cat-file -s "${OLD_BLOB}" >/dev/null
s -C "${SLEY_FILE_PARTIAL}" cat-file -t "${OLD_BLOB}" >/dev/null
s -C "${SLEY_FILE_PARTIAL}" cat-file -e "${OLD_BLOB}"

say "includeIf worktree"
INC=${ROOT}/incwt
LINKED=${ROOT}/inc-linked
PREFIX=${ROOT}/wt-prefix/linked
BARE=${ROOT}/wt-bare
g init -q -b main "${INC}"
printf 'f\n' >"${INC}/f"
g -C "${INC}" add f
g -C "${INC}" -c commit.gpgsign=false commit -qm init
g -C "${INC}" worktree add -q -b linked "${LINKED}" HEAD
g -C "${INC}" worktree add -q -b prefix "${PREFIX}" HEAD
MAIN_ABS=$(cd "${INC}" && pwd)
LINKED_ABS=$(cd "${LINKED}" && pwd)
PREFIX_PARENT=$(cd "${ROOT}/wt-prefix" && pwd)
MAIN_UPPER=$(printf '%s' "${MAIN_ABS}" | tr '[:lower:]' '[:upper:]')
cat >>"${INC}/.git/config" <<EOF
[includeIf "worktree:${MAIN_ABS}"]
	path = main-inc
[includeIf "worktree:${LINKED_ABS}"]
	path = linked-inc
[includeIf "worktree:${PREFIX_PARENT}/"]
	path = prefix-inc
[includeIf "worktree/i:${MAIN_UPPER}"]
	path = icase-inc
EOF
printf '[test]\n\tmainvar = main\n' >"${INC}/.git/main-inc"
printf '[test]\n\tlinkedvar = linked\n' >"${INC}/.git/linked-inc"
printf '[test]\n\tprefixvar = prefix\n' >"${INC}/.git/prefix-inc"
printf '[test]\n\ticase = 1\n' >"${INC}/.git/icase-inc"
test "$(s -C "${INC}" config test.mainvar)" = main
test "$(g -C "${INC}" config test.mainvar)" = main
test "$(s -C "${INC}" config test.icase)" = 1
test "$(g -C "${INC}" config test.icase)" = 1
if s -C "${INC}" config test.linkedvar >/dev/null 2>&1; then
    echo "main worktree matched the linked include" >&2
    exit 1
fi
test "$(s -C "${LINKED}" config test.linkedvar)" = linked
test "$(g -C "${LINKED}" config test.linkedvar)" = linked
if s -C "${LINKED}" config test.mainvar >/dev/null 2>&1; then
    echo "linked worktree matched the main include" >&2
    exit 1
fi
test "$(s -C "${PREFIX}" config test.prefixvar)" = prefix
test "$(g -C "${PREFIX}" config test.prefixvar)" = prefix
g init -q --bare "${BARE}"
printf '[includeIf "worktree:/"]\n\tpath = bar-bare\n' >>"${BARE}/config"
printf '[test]\n\twtbare = 1\n' >"${BARE}/bar-bare"
if s -C "${BARE}" config test.wtbare >/dev/null 2>&1; then
    echo "bare repo matched includeIf worktree:/" >&2
    exit 1
fi
if g -C "${BARE}" config test.wtbare >/dev/null 2>&1; then
    echo "stock git bare repo matched includeIf worktree:/ (oracle drift)" >&2
    exit 1
fi

say "config lock retry"
LOCK=${ROOT}/lockrepo
g init -q -b main "${LOCK}"
g -C "${LOCK}" config set keep.me 1
LOCKFILE=${LOCK}/.git/config.lock
touch "${LOCKFILE}"
(
    sleep 0.3
    rm -f "${LOCKFILE}"
) &
s -C "${LOCK}" config k.yes 1
wait
test "$(g -C "${LOCK}" config k.yes)" = 1
touch "${LOCKFILE}"
start_ns=$(date +%s%N)
set +e
s -C "${LOCK}" -c core.configLockTimeout=0 config k.no 1 >/dev/null 2>"${ROOT}/lock-timeout.err"
lock_rc=$?
set -e
end_ns=$(date +%s%N)
rm -f "${LOCKFILE}"
if [[ "${lock_rc}" -eq 0 ]]; then
    echo "configLockTimeout=0 acquired a held lock" >&2
    exit 1
fi
elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))
if [[ "${elapsed_ms}" -ge 800 ]]; then
    echo "configLockTimeout=0 waited ${elapsed_ms}ms" >&2
    exit 1
fi
if g -C "${LOCK}" config k.no >/dev/null 2>&1; then
    echo "failed lock write still stored k.no" >&2
    exit 1
fi
touch "${LOCKFILE}"
(
    sleep 0.25
    printf '\n[injected]\n\tfromstock = 1\n' >>"${LOCK}/.git/config"
    rm -f "${LOCKFILE}"
) &
s -C "${LOCK}" config sley.side 1
wait
test "$(g -C "${LOCK}" config keep.me)" = 1
test "$(g -C "${LOCK}" config injected.fromstock)" = 1
test "$(g -C "${LOCK}" config sley.side)" = 1
g -C "${LOCK}" config --list >/dev/null

say "write-tree directory/file conflict"
DF=${ROOT}/df-index
g init -q -b main "${DF}"
BLOB=$(printf 'blob\n' | g -C "${DF}" hash-object -w --stdin)
python3 - "${DF}/.git/index" "${BLOB}" <<'PY'
import hashlib, struct, sys
path, oid_hex = sys.argv[1], sys.argv[2]
oid = bytes.fromhex(oid_hex)
names = [b"a", b"a/b", b"docs", b"docs-internal/README.md", b"docs/requirements.txt"]
out = bytearray(b"DIRC")
out += struct.pack(">II", 2, len(names))
for name in names:
    start = len(out)
    out += struct.pack(">10I", 0, 0, 0, 0, 0, 0, 0o100644, 0, 0, len(b"blob\n"))
    out += oid
    out += struct.pack(">H", len(name))
    out += name + b"\0"
    while (len(out) - start) % 8:
        out.append(0)
out += hashlib.sha1(out).digest()
open(path, "wb").write(out)
PY
set +e
g -C "${DF}" write-tree >"${ROOT}/df-stock.out" 2>"${ROOT}/df-stock.err"
stock_rc=$?
s -C "${DF}" write-tree >"${ROOT}/df-sley.out" 2>"${ROOT}/df-sley.err"
sley_rc=$?
set -e
if [[ "${stock_rc}" -eq 0 || "${sley_rc}" -eq 0 ]]; then
    echo "write-tree accepted a D/F index (stock=${stock_rc} sley=${sley_rc})" >&2
    echo "stock:" >&2
    cat "${ROOT}/df-stock.err" >&2
    echo "sley:" >&2
    cat "${ROOT}/df-sley.err" >&2
    exit 1
fi
if ! cmp -s "${ROOT}/df-stock.err" "${ROOT}/df-sley.err"; then
    echo "write-tree D/F stderr differs" >&2
    diff -u "${ROOT}/df-stock.err" "${ROOT}/df-sley.err" >&2 || true
    exit 1
fi
grep -q "You have both docs and docs/requirements.txt" "${ROOT}/df-sley.err"
grep -q "You have both a and a/b" "${ROOT}/df-sley.err"
grep -q "fatal: git-write-tree: error building trees" "${ROOT}/df-sley.err"

say "object-info"
OI=${ROOT}/oi-client
OI_EXEC=${ROOT}/oi-exec
mkdir -p "${OI_EXEC}"
cat >"${OI_EXEC}/git-upload-pack" <<EOF
#!/bin/sh
exec "${SLEY}" upload-pack "\$@"
EOF
chmod +x "${OI_EXEC}/git-upload-pack"
g clone -q --no-local -u "${UPLOAD}" "file://${SRV}" "${OI}"
BLOB_OID=$(g -C "${SRV}" rev-parse HEAD:a)
COMMIT_OID=$(g -C "${SRV}" rev-parse HEAD)
MISSING=1111111111111111111111111111111111111111
if g -C "${SRV}" cat-file -e "${MISSING}" >/dev/null 2>&1; then
    echo "missing oid unexpectedly exists" >&2
    exit 1
fi
FMT='%(objectname) %(objecttype) %(objectsize)'
oi_query() {
    printf 'remote-object-info origin %s %s %s\n' "${BLOB_OID}" "${COMMIT_OID}" "${MISSING}"
}
if GIT_EXEC_PATH="${OI_EXEC}" g -C "${OI}" cat-file --batch-command="${FMT}" >"${ROOT}/oi-off.out" 2>"${ROOT}/oi-off.err" < <(oi_query); then
    echo "object-info succeeded while transfer.advertiseObjectInfo was unset" >&2
    exit 1
fi
grep -q "object-info capability is not enabled" "${ROOT}/oi-off.err"
GIT_TRACE_PACKET=${ROOT}/oi-caps GIT_EXEC_PATH="${OI_EXEC}" g -C "${OI}" ls-remote origin >/dev/null
if grep -q object-info "${ROOT}/oi-caps"; then
    echo "sley advertised object-info by default" >&2
    exit 1
fi
s -C "${SRV}" config transfer.advertiseObjectInfo true
GIT_EXEC_PATH="${OI_EXEC}" g -C "${OI}" cat-file --batch-command="${FMT}" >"${ROOT}/oi-sley.out" 2>"${ROOT}/oi-sley.err" < <(oi_query)
g -C "${OI}" cat-file --batch-command="${FMT}" >"${ROOT}/oi-stock.out" 2>"${ROOT}/oi-stock.err" < <(oi_query)
if ! cmp -s "${ROOT}/oi-sley.out" "${ROOT}/oi-stock.out"; then
    echo "object-info answers differ" >&2
    diff -u "${ROOT}/oi-stock.out" "${ROOT}/oi-sley.out" >&2 || true
    echo "sley stderr:" >&2
    cat "${ROOT}/oi-sley.err" >&2
    echo "stock stderr:" >&2
    cat "${ROOT}/oi-stock.err" >&2
    exit 1
fi
SMALL=$(printf 'x' | g -C "${SRV}" hash-object -w --stdin)
BIG=$(printf 'yyyyyyyy\n' | g -C "${SRV}" hash-object -w --stdin)
g -C "${SRV}" replace "${SMALL}" "${BIG}"
# A normal reader follows the replace ref. upload-pack disables replace refs,
# so both servers must still report the stored object's size.
test "$(g -C "${SRV}" cat-file -s "${SMALL}")" = 9
printf 'remote-object-info origin %s\n' "${SMALL}" >"${ROOT}/oi-replace.in"
GIT_EXEC_PATH="${OI_EXEC}" g -C "${OI}" cat-file --batch-command="${FMT}" <"${ROOT}/oi-replace.in" >"${ROOT}/oi-repl-sley.out"
g -C "${OI}" cat-file --batch-command="${FMT}" <"${ROOT}/oi-replace.in" >"${ROOT}/oi-repl-stock.out"
if ! cmp -s "${ROOT}/oi-repl-sley.out" "${ROOT}/oi-repl-stock.out"; then
    echo "replaced object-info answers differ" >&2
    diff -u "${ROOT}/oi-repl-stock.out" "${ROOT}/oi-repl-sley.out" >&2 || true
    exit 1
fi
grep -qx "${SMALL} blob 1" "${ROOT}/oi-repl-sley.out"

say "interop ok"
