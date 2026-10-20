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

# The partial-clone section exports a simple SSH variant, which never sends
# GIT_PROTOCOL. Protocol v2 tests below select the OpenSSH variant explicitly.
unset GIT_SSH_VARIANT || true

trace_has_ls_refs() {
    local trace=$1
    local label=$2
    if ! grep -q 'command=ls-refs' "${trace}"; then
        echo "${label}: packet trace missing command=ls-refs" >&2
        cat "${trace}" >&2 || true
        exit 1
    fi
}

trace_lacks_ls_refs() {
    local trace=$1
    local label=$2
    if grep -q 'command=ls-refs' "${trace}"; then
        echo "${label}: packet trace contained command=ls-refs" >&2
        exit 1
    fi
}

say "sley client protocol v2 over ssh against stock upload-pack"
g -C "${STOCK}" tag -a interop-tag -m 'interop tag' HEAD
SSH_URL="ssh://git@127.0.0.1${STOCK}"
SLEY_V2_LS=${ROOT}/sley-ssh-v2-ls
STOCK_V2_LS=${ROOT}/stock-v2-ls
SSH_V2_TRACE=${ROOT}/ssh-v2-ls.trace
GIT_TRACE_PACKET="${SSH_V2_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s ls-remote "${SSH_URL}" >"${SLEY_V2_LS}"
g ls-remote "${STOCK}" >"${STOCK_V2_LS}"
diff -u "${STOCK_V2_LS}" "${SLEY_V2_LS}"
trace_has_ls_refs "${SSH_V2_TRACE}" "ssh v2 ls-remote"
grep -F -q 'refs/tags/interop-tag' "${SLEY_V2_LS}"

SLEY_SSH_V2=${ROOT}/sley-ssh-v2
SSH_V2_CLONE_TRACE=${ROOT}/ssh-v2-clone.trace
GIT_TRACE_PACKET="${SSH_V2_CLONE_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s clone -q -u "${GU}" "${SSH_URL}" "${SLEY_SSH_V2}"
trace_has_ls_refs "${SSH_V2_CLONE_TRACE}" "ssh v2 clone"
same_commit "${SLEY_SSH_V2}" "${WT}" "ssh v2 clone"

STOCK_WT=${ROOT}/stock-wt
g clone -q "${STOCK}" "${STOCK_WT}"
printf 'v2fetch\n' >"${STOCK_WT}/v2fetch"
g -C "${STOCK_WT}" add v2fetch
GIT_AUTHOR_DATE="${EQUAL}" GIT_COMMITTER_DATE="${EQUAL}" \
    g -C "${STOCK_WT}" -c commit.gpgsign=false commit -qm v2fetch
g -C "${STOCK_WT}" push -q origin HEAD:refs/heads/main
SSH_V2_FETCH_TRACE=${ROOT}/ssh-v2-fetch.trace
GIT_TRACE_PACKET="${SSH_V2_FETCH_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s -C "${SLEY_SSH_V2}" fetch -q origin
trace_has_ls_refs "${SSH_V2_FETCH_TRACE}" "ssh v2 fetch"
test "$(g -C "${SLEY_SSH_V2}" rev-parse origin/main)" = "$(g -C "${STOCK_WT}" rev-parse HEAD)"

SLEY_SSH_SHALLOW=${ROOT}/sley-ssh-shallow
SSH_SHALLOW_TRACE=${ROOT}/ssh-shallow.trace
GIT_TRACE_PACKET="${SSH_SHALLOW_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s clone -q --depth=1 -u "${GU}" "${SSH_URL}" "${SLEY_SSH_SHALLOW}"
trace_has_ls_refs "${SSH_SHALLOW_TRACE}" "ssh v2 shallow clone"
test "$(g -C "${SLEY_SSH_SHALLOW}" rev-list --count HEAD)" = 1

SLEY_SSH_V2_PARTIAL=${ROOT}/sley-ssh-v2-partial
SSH_PARTIAL_TRACE=${ROOT}/ssh-v2-partial.trace
GIT_TRACE_PACKET="${SSH_PARTIAL_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s clone -q --filter=blob:none -u "${GU}" "${SSH_URL}" "${SLEY_SSH_V2_PARTIAL}"
trace_has_ls_refs "${SSH_PARTIAL_TRACE}" "ssh v2 partial clone"
test -f "${SLEY_SSH_V2_PARTIAL}/a"
if GIT_NO_LAZY_FETCH=1 g -C "${SLEY_SSH_V2_PARTIAL}" cat-file -e "${OLD_BLOB}" >/dev/null 2>&1; then
    echo "ssh v2 partial clone contained historical blob ${OLD_BLOB}" >&2
    exit 1
fi
GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s -C "${SLEY_SSH_V2_PARTIAL}" cat-file -s "${OLD_BLOB}" >/dev/null

say "ssh protocol.version and GIT_PROTOCOL overrides"
SSH_V0_TRACE=${ROOT}/ssh-v0-config.trace
GIT_TRACE_PACKET="${SSH_V0_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s -c protocol.version=0 ls-remote "${SSH_URL}" >"${ROOT}/sley-ssh-v0-ls"
trace_lacks_ls_refs "${SSH_V0_TRACE}" "ssh protocol.version=0"
diff -u <(g -c protocol.version=0 ls-remote "${STOCK}") "${ROOT}/sley-ssh-v0-ls"

SSH_ENV_V0_TRACE=${ROOT}/ssh-env-v0.trace
GIT_PROTOCOL=version=0 GIT_TRACE_PACKET="${SSH_ENV_V0_TRACE}" \
    GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s ls-remote "${SSH_URL}" >/dev/null
trace_lacks_ls_refs "${SSH_ENV_V0_TRACE}" "GIT_PROTOCOL=version=0"

SSH_ENV_V2_TRACE=${ROOT}/ssh-env-v2.trace
GIT_PROTOCOL=version=2 GIT_TRACE_PACKET="${SSH_ENV_V2_TRACE}" \
    GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s -c protocol.version=0 ls-remote "${SSH_URL}" >/dev/null
trace_has_ls_refs "${SSH_ENV_V2_TRACE}" "GIT_PROTOCOL overrides protocol.version=0"

SLEY_SSH_V1=${ROOT}/sley-ssh-v1
SSH_V1_TRACE=${ROOT}/ssh-v1.trace
GIT_TRACE_PACKET="${SSH_V1_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s -c protocol.version=1 clone -q -u "${GU}" "${SSH_URL}" "${SLEY_SSH_V1}"
trace_lacks_ls_refs "${SSH_V1_TRACE}" "ssh protocol.version=1"
same_commit "${SLEY_SSH_V1}" "${STOCK_WT}" "ssh protocol.version=1 clone"

say "ssh v2 request falls back when stock upload-pack speaks v0"
V0_UP=${ROOT}/v0-upload-pack
cat >"${V0_UP}" <<EOF
#!/bin/sh
unset GIT_PROTOCOL
exec "${GU}" "\$@"
EOF
chmod +x "${V0_UP}"
SLEY_SSH_FALLBACK=${ROOT}/sley-ssh-v0-fallback
SSH_FALLBACK_TRACE=${ROOT}/ssh-v0-fallback.trace
GIT_TRACE_PACKET="${SSH_FALLBACK_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s clone -q -u "${V0_UP}" "${SSH_URL}" "${SLEY_SSH_FALLBACK}"
trace_lacks_ls_refs "${SSH_FALLBACK_TRACE}" "ssh v0 fallback"
same_commit "${SLEY_SSH_FALLBACK}" "${STOCK_WT}" "ssh v0 fallback clone"
SSH_FALLBACK_SHALLOW=${ROOT}/sley-ssh-v0-fallback-shallow
GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s -c protocol.version=0 clone -q --depth=1 -u "${GU}" "${SSH_URL}" "${SSH_FALLBACK_SHALLOW}"
test "$(g -C "${SSH_FALLBACK_SHALLOW}" rev-list --count HEAD)" = 1

printf 'from-sley\n' >"${SLEY_SSH_V2}/from-sley"
s -C "${SLEY_SSH_V2}" add from-sley
GIT_AUTHOR_DATE="${EQUAL}" GIT_COMMITTER_DATE="${EQUAL}" \
    s -C "${SLEY_SSH_V2}" -c user.name='Interop User' -c user.email='interop@example.invalid' \
    -c commit.gpgsign=false commit -qm from-sley
GR="$(dirname "${GU}")/git-receive-pack"
SSH_PUSH_TRACE=${ROOT}/ssh-push.trace
GIT_TRACE_PACKET="${SSH_PUSH_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    s -C "${SLEY_SSH_V2}" push -q --receive-pack="${GR}" origin HEAD:refs/heads/from-sley
trace_lacks_ls_refs "${SSH_PUSH_TRACE}" "ssh push stays v0"
test "$(g -C "${STOCK}" rev-parse from-sley)" = "$(g -C "${SLEY_SSH_V2}" rev-parse HEAD)"

say "sley client protocol v2 over git:// against stock git daemon"
DPORT=$(free_port)
DAEMON_LOG=${ROOT}/stock-daemon.log
"${GIT}" daemon --listen=127.0.0.1 --port="${DPORT}" --export-all --reuseaddr \
    --informative-errors --verbose --base-path="${ROOT}" "${ROOT}" >"${DAEMON_LOG}" 2>&1 &
DAEMON_PID=$!
wait_for_log "${DAEMON_PID}" "${DAEMON_LOG}" "Ready to rumble"
GIT_URL="git://127.0.0.1:${DPORT}/stock.git"
GIT_V2_TRACE=${ROOT}/git-v2-ls.trace
GIT_TRACE_PACKET="${GIT_V2_TRACE}" s ls-remote "${GIT_URL}" >"${ROOT}/sley-git-v2-ls"
g ls-remote "${STOCK}" >"${ROOT}/stock-git-v2-ls"
diff -u "${ROOT}/stock-git-v2-ls" "${ROOT}/sley-git-v2-ls"
trace_has_ls_refs "${GIT_V2_TRACE}" "git:// v2 ls-remote"

SLEY_GIT_V2=${ROOT}/sley-git-v2
GIT_V2_CLONE_TRACE=${ROOT}/git-v2-clone.trace
GIT_TRACE_PACKET="${GIT_V2_CLONE_TRACE}" s clone -q "${GIT_URL}" "${SLEY_GIT_V2}"
trace_has_ls_refs "${GIT_V2_CLONE_TRACE}" "git:// v2 clone"
same_commit "${SLEY_GIT_V2}" "${STOCK_WT}" "git:// v2 clone"
GIT_V2_FETCH_TRACE=${ROOT}/git-v2-fetch.trace
printf 'daemon-fetch\n' >"${STOCK_WT}/daemon-fetch"
g -C "${STOCK_WT}" add daemon-fetch
GIT_AUTHOR_DATE="${EQUAL}" GIT_COMMITTER_DATE="${EQUAL}" \
    g -C "${STOCK_WT}" -c commit.gpgsign=false commit -qm daemon-fetch
g -C "${STOCK_WT}" push -q origin HEAD:refs/heads/main
GIT_TRACE_PACKET="${GIT_V2_FETCH_TRACE}" s -C "${SLEY_GIT_V2}" fetch -q origin
trace_has_ls_refs "${GIT_V2_FETCH_TRACE}" "git:// v2 fetch"
test "$(g -C "${SLEY_GIT_V2}" rev-parse origin/main)" = "$(g -C "${STOCK_WT}" rev-parse HEAD)"

SLEY_GIT_SHALLOW=${ROOT}/sley-git-shallow
GIT_SHALLOW_TRACE=${ROOT}/git-shallow.trace
GIT_TRACE_PACKET="${GIT_SHALLOW_TRACE}" \
    s clone -q --depth=1 "${GIT_URL}" "${SLEY_GIT_SHALLOW}"
trace_has_ls_refs "${GIT_SHALLOW_TRACE}" "git:// v2 shallow clone"
test "$(g -C "${SLEY_GIT_SHALLOW}" rev-list --count HEAD)" = 1

SLEY_GIT_PARTIAL=${ROOT}/sley-git-partial
GIT_PARTIAL_TRACE=${ROOT}/git-partial.trace
GIT_TRACE_PACKET="${GIT_PARTIAL_TRACE}" \
    s clone -q --filter=blob:none "${GIT_URL}" "${SLEY_GIT_PARTIAL}"
trace_has_ls_refs "${GIT_PARTIAL_TRACE}" "git:// v2 partial clone"
test -f "${SLEY_GIT_PARTIAL}/a"
if GIT_NO_LAZY_FETCH=1 g -C "${SLEY_GIT_PARTIAL}" cat-file -e "${OLD_BLOB}" >/dev/null 2>&1; then
    echo "git:// v2 partial clone contained historical blob ${OLD_BLOB}" >&2
    exit 1
fi

GIT_V0_CFG_TRACE=${ROOT}/git-v0-config.trace
GIT_TRACE_PACKET="${GIT_V0_CFG_TRACE}" \
    s -c protocol.version=0 ls-remote "${GIT_URL}" >"${ROOT}/sley-git-v0-ls"
trace_lacks_ls_refs "${GIT_V0_CFG_TRACE}" "git:// protocol.version=0"
diff -u <(g -c protocol.version=0 ls-remote "${STOCK}") "${ROOT}/sley-git-v0-ls"

kill "${DAEMON_PID}" 2>/dev/null || true
wait "${DAEMON_PID}" 2>/dev/null || true
DAEMON_PID=

say "git:// v2 request falls back when stock upload-pack speaks v0"
V0_EXEC=${ROOT}/v0-stock-exec
mkdir -p "${V0_EXEC}"
STOCK_EXEC_PATH="$("${GIT}" --exec-path)"
for helper in "${STOCK_EXEC_PATH}/"*; do
    ln -s "${helper}" "${V0_EXEC}/$(basename "${helper}")"
done
rm -f "${V0_EXEC}/git" "${V0_EXEC}/git-upload-pack"
# git-daemon runs the upload-pack builtin via `git upload-pack`, resolved
# from GIT_EXEC_PATH. Strip the client's version=2 before that builtin.
cat >"${V0_EXEC}/git" <<EOF
#!/bin/sh
if [ "\$1" = "upload-pack" ]; then
    unset GIT_PROTOCOL
fi
exec "${GIT}" "\$@"
EOF
chmod +x "${V0_EXEC}/git"
cat >"${V0_EXEC}/git-upload-pack" <<EOF
#!/bin/sh
unset GIT_PROTOCOL
exec "${GU}" "\$@"
EOF
chmod +x "${V0_EXEC}/git-upload-pack"
DPORT=$(free_port)
DAEMON_LOG=${ROOT}/stock-v0-daemon.log
GIT_EXEC_PATH="${V0_EXEC}" "${GIT}" daemon --listen=127.0.0.1 --port="${DPORT}" \
    --export-all --reuseaddr --informative-errors --verbose \
    --base-path="${ROOT}" "${ROOT}" >"${DAEMON_LOG}" 2>&1 &
DAEMON_PID=$!
wait_for_log "${DAEMON_PID}" "${DAEMON_LOG}" "Ready to rumble"
GIT_URL="git://127.0.0.1:${DPORT}/stock.git"
SLEY_GIT_FALLBACK=${ROOT}/sley-git-v0-fallback
GIT_FALLBACK_TRACE=${ROOT}/git-v0-fallback.trace
GIT_TRACE_PACKET="${GIT_FALLBACK_TRACE}" s clone -q "${GIT_URL}" "${SLEY_GIT_FALLBACK}"
trace_lacks_ls_refs "${GIT_FALLBACK_TRACE}" "git:// v0 fallback"
same_commit "${SLEY_GIT_FALLBACK}" "${STOCK_WT}" "git:// v0 fallback clone"
kill "${DAEMON_PID}" 2>/dev/null || true
wait "${DAEMON_PID}" 2>/dev/null || true
DAEMON_PID=

say "stock git client protocol v2 over ssh against sley upload-pack"
STOCK_SSH_V2=${ROOT}/stock-ssh-v2
STOCK_SSH_TRACE=${ROOT}/stock-ssh-v2.trace
GIT_TRACE_PACKET="${STOCK_SSH_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    g clone -q -u "${UPLOAD}" "ssh://git@127.0.0.1${SRV}" "${STOCK_SSH_V2}"
trace_has_ls_refs "${STOCK_SSH_TRACE}" "stock ssh client v2"
# WT moved on (the a2 commit) without being pushed to SRV. The clone must
# match the server it talked to.
same_commit "${STOCK_SSH_V2}" "${SRV}" "stock ssh v2 clone"
s -C "${WT}" push -q "${SRV}" main
STOCK_SSH_FETCH_TRACE=${ROOT}/stock-ssh-v2-fetch.trace
GIT_TRACE_PACKET="${STOCK_SSH_FETCH_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    g -C "${STOCK_SSH_V2}" fetch -q --upload-pack="${UPLOAD}" origin
trace_has_ls_refs "${STOCK_SSH_FETCH_TRACE}" "stock ssh client v2 fetch"
test "$(g -C "${STOCK_SSH_V2}" rev-parse origin/main)" = "$(g -C "${SRV}" rev-parse HEAD)"
STOCK_SSH_V0=${ROOT}/stock-ssh-v0
STOCK_SSH_V0_TRACE=${ROOT}/stock-ssh-v0.trace
GIT_TRACE_PACKET="${STOCK_SSH_V0_TRACE}" GIT_SSH_COMMAND="${FAKE_SSH}" GIT_SSH_VARIANT=ssh \
    g -c protocol.version=0 clone -q -u "${UPLOAD}" "ssh://git@127.0.0.1${SRV}" "${STOCK_SSH_V0}"
trace_lacks_ls_refs "${STOCK_SSH_V0_TRACE}" "stock ssh client v0"
same_commit "${STOCK_SSH_V0}" "${SRV}" "stock ssh v0 clone"

say "interop ok"
