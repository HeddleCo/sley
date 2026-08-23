#!/usr/bin/env bash
# Asserter for the crates.io publish pipeline's crate-list contract.
#
# Mirrors heddle's scripts/check-publish-pipeline.sh (heddle#72). The publish
# workflow (.github/workflows/publish-crates.yml) carries an EXPLICIT list of
# publishable crates in a topological publish order:
#
#     CRATES=(
#       sley-core
#       ...
#       sley
#     )
#
# The list is kept explicit — NOT auto-discovered from `cargo metadata` at
# publish time — on purpose: a crate flipping `publish = true` must not silently
# expand the public crates.io surface without a PR-review signal (heddle#72).
# The trade-off is that an explicit list can go stale (sley 0.8's publish broke
# twice this way: a stale topo order, then a facade -> publish=false dep). This
# asserter closes that gap by running in PR CI and cross-checking the explicit
# list against the live `cargo metadata`:
#
#   - membership: CRATES must equal the actual publishable set (workspace
#     members whose `publish` is not `false`). A publishable crate missing from
#     the list is a silent surface addition; a listed crate that isn't
#     publishable (renamed / unpublished / typo) is dead weight that will fail
#     the publish run.
#   - topological validity: every crate must appear after all of its
#     in-workspace publishable dependencies, so each `cargo publish` can resolve
#     its deps against crates.io.
#   - no duplicate entries.
#
# NOTE ON SCOPE: sley's publish workflow is a manual `workflow_dispatch` (a human
# picks dry-run/real + version), NOT heddle's push-to-main release-plz flow. So
# this asserter intentionally does NOT mirror heddle's trust-gate structural
# checks (validate-publish job / SHA-pin / no-workflow_dispatch) — those encode
# heddle's automated-publish threat model, which does not apply here. What DOES
# transfer, and is mirrored closely, is the explicit-list-guard.

set -euo pipefail

WF=".github/workflows/publish-crates.yml"
fail=0

err() { echo "::error::$*" >&2; fail=1; }
ok()  { echo "ok: $*"; }

if [[ ! -f "$WF" ]]; then
  err "$WF does not exist"
  exit 1
fi

# --- Smoke (grep) ---------------------------------------------------------

# Explicit crate list. Auto-discovery via `cargo metadata` at publish time
# would publish whatever's currently marked publishable in Cargo.toml, which is
# invisible at PR review time. An explicit CRATES=(...) array makes adding a
# publishable crate a reviewed one-line workflow edit. We look for the marker.
if grep -E "^\s*CRATES=\(\s*$" "$WF" >/dev/null; then
  ok "explicit CRATES=( ... ) list present"
else
  err "$WF must maintain an explicit CRATES=( ... ) list (no auto-discovery — see heddle#72 design)"
fi

# Token wiring. The repo-settings secret is CRATES_IO_API_KEY; cargo reads
# CARGO_REGISTRY_TOKEN from the process env. The workflow maps one to the other.
# A rename on either side would silently break authentication, so check both.
if grep -F 'secrets.CRATES_IO_API_KEY' "$WF" >/dev/null; then
  ok "workflow reads secrets.CRATES_IO_API_KEY"
else
  err "$WF must reference secrets.CRATES_IO_API_KEY (the configured repo-settings secret name)"
fi

if grep -E '^\s*CARGO_REGISTRY_TOKEN:' "$WF" >/dev/null; then
  ok "CARGO_REGISTRY_TOKEN env var declared"
else
  err "$WF must expose the token as the CARGO_REGISTRY_TOKEN env var (cargo's documented name)"
fi

# --- Strict list check (cargo metadata cross-check) -----------------------
#
# Re-derive the publishable set + in-workspace dependency edges from the live
# `cargo metadata --no-deps` and cross-check the workflow's explicit CRATES
# list against it. `--no-deps` keeps the set to workspace members and does not
# resolve third-party deps (fast, offline).

if ! command -v python3 >/dev/null 2>&1; then
  err "python3 not available; publish-list check skipped"
elif ! command -v cargo >/dev/null 2>&1; then
  err "cargo not available; publish-list check skipped"
else
  META_FILE="$(mktemp)"
  trap 'rm -f "$META_FILE"' EXIT
  if ! cargo metadata --format-version 1 --no-deps > "$META_FILE"; then
    err "cargo metadata failed; cannot validate the publish list"
  else
    list_report=$(python3 - "$WF" "$META_FILE" <<'PY'
import json
import re
import sys
from pathlib import Path

wf_lines = Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
meta = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))

errors = []
oks = []

# Publishable set + in-workspace publishable dependency edges. `publish == []`
# means `publish = false`; anything else (unset, true, non-empty registry list)
# is publishable.
pub = {p["name"] for p in meta["packages"] if p.get("publish") != []}
deps = {
    p["name"]: {
        d["name"]
        for d in p["dependencies"]
        if d["name"] in pub and d["name"] != p["name"]
    }
    for p in meta["packages"]
    if p["name"] in pub
}


def check(crates, pub, deps):
    """Pure validator: returns (errors, oks) for an explicit publish list
    against a publishable set and its in-set dependency edges."""
    errs, oks = [], []

    if not crates:
        errs.append("could not parse the CRATES=( ... ) array from the workflow")
        return errs, oks

    # Duplicates.
    if len(set(crates)) != len(crates):
        dups = sorted({c for c in crates if crates.count(c) > 1})
        errs.append(f"CRATES contains duplicate entries: {dups}")

    listed = set(crates)
    missing = sorted(pub - listed)   # publishable but not listed -> silent surface / stale
    extra = sorted(listed - pub)     # listed but not publishable -> renamed / unpublished / typo

    if missing:
        errs.append(
            "CRATES is stale: publishable crate(s) missing from the list: "
            f"{missing} — a crate became publishable without a PR-review signal; "
            "add it to CRATES (in a valid topological position) or set publish=false"
        )
    if extra:
        errs.append(
            "CRATES lists crate(s) that are not publishable workspace members: "
            f"{extra} — remove them (renamed / unpublished / typo)"
        )

    if not missing and not extra and len(set(crates)) == len(crates):
        oks.append(f"CRATES matches the publishable set exactly ({len(pub)} crates)")

    # Topological validity: every crate after all of its in-set deps.
    pos = {c: i for i, c in enumerate(crates)}
    order_ok = True
    edges = 0
    for c in crates:
        if c not in deps:
            # unknown crate — membership check already flagged it
            continue
        for d in deps[c]:
            if d in pos:
                edges += 1
                if pos[d] >= pos[c]:
                    order_ok = False
                    errs.append(
                        f"CRATES order invalid: {c} depends on {d}, "
                        "which must appear earlier in the list"
                    )
    if order_ok and not missing and not extra:
        oks.append(f"CRATES is topologically ordered ({edges} in-set dependency edges)")

    return errs, oks


# --- Self-test the validator ---------------------------------------------
# Prove the checker can FAIL. A future edit that neuters it fails here first.
def _selftest():
    tpub = {"a", "b", "c"}
    tdeps = {"a": set(), "b": {"a"}, "c": {"a", "b"}}
    # Correct list passes clean.
    e, _ = check(["a", "b", "c"], tpub, tdeps)
    if e:
        errors.append(f"validator self-test failed: correct list rejected ({e})")
    # Dropped publishable crate is caught (membership).
    e, _ = check(["a", "b"], tpub, tdeps)
    if not any("missing from the list" in x for x in e):
        errors.append("validator self-test failed: dropped crate not caught")
    # Bogus crate is caught (membership).
    e, _ = check(["a", "b", "c", "bogus"], tpub, tdeps)
    if not any("not publishable" in x for x in e):
        errors.append("validator self-test failed: bogus crate not caught")
    # Dep after dependent is caught (order).
    e, _ = check(["b", "a", "c"], tpub, tdeps)
    if not any("order invalid" in x for x in e):
        errors.append("validator self-test failed: mis-order not caught")
    # Duplicate is caught.
    e, _ = check(["a", "b", "b", "c"], tpub, tdeps)
    if not any("duplicate" in x for x in e):
        errors.append("validator self-test failed: duplicate not caught")


_selftest()
if not any("self-test failed" in x for x in errors):
    oks.append("validator self-test: correct passes; drop / bogus / mis-order / dup all fail")

# --- Parse CRATES=(...) from the workflow --------------------------------
crates = []
in_arr = False
for line in wf_lines:
    s = line.strip()
    if not in_arr:
        if re.match(r"CRATES=\(\s*$", s):
            in_arr = True
        continue
    if s.startswith(")"):
        break
    if s and not s.startswith("#"):
        crates.append(s)

list_errs, list_oks = check(crates, pub, deps)
errors.extend(list_errs)
oks.extend(list_oks)

print("OKS:")
for o in oks:
    print(o)
print("ERRORS:")
for e in errors:
    print(e)
PY
    )

    in_oks=0
    in_errors=0
    while IFS= read -r line; do
      case "$line" in
        "OKS:")    in_oks=1; in_errors=0; continue ;;
        "ERRORS:") in_oks=0; in_errors=1; continue ;;
      esac
      [[ -z "$line" ]] && continue
      if (( in_oks )); then
        ok "$line"
      elif (( in_errors )); then
        err "$line"
      fi
    done <<< "$list_report"
  fi
fi

if (( fail )); then
  echo "publish-pipeline check FAILED" >&2
  exit 1
fi
echo "publish-pipeline check passed"
