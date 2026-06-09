#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! command -v hyperfine >/dev/null 2>&1; then
  echo "hyperfine is required (https://github.com/sharkdp/hyperfine)" >&2
  exit 1
fi

if ! command -v git >/dev/null 2>&1; then
  echo "system git is required" >&2
  exit 1
fi

echo "Building release sley..." >&2
cargo build -p sley-cli --bin sley --release --quiet

SLEY="$ROOT/target/release/sley"

if [[ -n "${SLEY_BENCH_PACK_REPO:-}" && -n "${SLEY_BENCH_COMMIT_REPO:-}" ]]; then
  echo "Using fixture paths from environment" >&2
else
  echo "Creating benchmark fixtures..." >&2
  # shellcheck disable=SC1090
  eval "$(cargo run -p sley-bench --example setup_fixtures 2>/dev/null)"
fi

: "${SLEY_BENCH_PACK_REPO:?missing SLEY_BENCH_PACK_REPO}"
: "${SLEY_BENCH_PACK_SAMPLE_OID:?missing SLEY_BENCH_PACK_SAMPLE_OID}"
: "${SLEY_BENCH_PACK_BATCH_FILE:?missing SLEY_BENCH_PACK_BATCH_FILE}"
: "${SLEY_BENCH_COMMIT_REPO:?missing SLEY_BENCH_COMMIT_REPO}"

HF_COMMON=(--warmup 5 --min-runs 10)

run_hyperfine() {
  local title="$1"
  shift
  echo >&2
  echo "== $title ==" >&2
  hyperfine "${HF_COMMON[@]}" "$@"
}

run_hyperfine "cat-file -p" \
  "git -C ${SLEY_BENCH_PACK_REPO@Q} cat-file -p ${SLEY_BENCH_PACK_SAMPLE_OID@Q}" \
  "${SLEY@Q} -C ${SLEY_BENCH_PACK_REPO@Q} cat-file -p ${SLEY_BENCH_PACK_SAMPLE_OID@Q}"

run_hyperfine "cat-file --batch-check (500 oids)" \
  "git -C ${SLEY_BENCH_PACK_REPO@Q} cat-file --batch-check < ${SLEY_BENCH_PACK_BATCH_FILE@Q}" \
  "${SLEY@Q} -C ${SLEY_BENCH_PACK_REPO@Q} cat-file --batch-check < ${SLEY_BENCH_PACK_BATCH_FILE@Q}"

run_hyperfine "cat-file --batch (500 oids)" \
  "git -C ${SLEY_BENCH_PACK_REPO@Q} cat-file --batch < ${SLEY_BENCH_PACK_BATCH_FILE@Q}" \
  "${SLEY@Q} -C ${SLEY_BENCH_PACK_REPO@Q} cat-file --batch < ${SLEY_BENCH_PACK_BATCH_FILE@Q}"

REV_PARSE_LOOP="while IFS= read -r oid; do git -C ${SLEY_BENCH_PACK_REPO@Q} rev-parse \"\$oid\" >/dev/null; done < ${SLEY_BENCH_PACK_BATCH_FILE@Q}"
REV_PARSE_LOOP_SLEY="while IFS= read -r oid; do ${SLEY@Q} -C ${SLEY_BENCH_PACK_REPO@Q} rev-parse \"\$oid\" >/dev/null; done < ${SLEY_BENCH_PACK_BATCH_FILE@Q}"

run_hyperfine "rev-parse loop (500 oids)" \
  "bash -c ${REV_PARSE_LOOP@Q}" \
  "bash -c ${REV_PARSE_LOOP_SLEY@Q}"

run_hyperfine "count-objects -v" \
  "git -C ${SLEY_BENCH_PACK_REPO@Q} count-objects -v" \
  "${SLEY@Q} -C ${SLEY_BENCH_PACK_REPO@Q} count-objects -v"

run_hyperfine "rev-list --count HEAD" \
  "git -C ${SLEY_BENCH_COMMIT_REPO@Q} rev-list --count HEAD" \
  "${SLEY@Q} -C ${SLEY_BENCH_COMMIT_REPO@Q} rev-list --count HEAD"

run_hyperfine "for-each-ref" \
  "git -C ${SLEY_BENCH_COMMIT_REPO@Q} for-each-ref" \
  "${SLEY@Q} -C ${SLEY_BENCH_COMMIT_REPO@Q} for-each-ref"

run_hyperfine "ls-tree -r HEAD" \
  "git -C ${SLEY_BENCH_COMMIT_REPO@Q} ls-tree -r HEAD" \
  "${SLEY@Q} -C ${SLEY_BENCH_COMMIT_REPO@Q} ls-tree -r HEAD"