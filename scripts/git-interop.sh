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

say "interop ok"
