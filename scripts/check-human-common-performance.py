#!/usr/bin/env python3
"""Gate common-command benchmark results against Git."""

from __future__ import annotations

import argparse
import csv
import math
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("summary", type=Path)
    parser.add_argument("--mode", default="sley_cli")
    parser.add_argument("--max-case-slower", type=float, default=1.05)
    parser.add_argument("--max-geomean-ratio", type=float, default=0.95)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    with args.summary.open(newline="", encoding="utf-8") as handle:
        rows = list(csv.DictReader(handle))

    git = {
        (row["repo_name"], row["command_name"]): row
        for row in rows
        if row.get("mode") == "git" and row.get("mean_ms")
    }
    candidate = {
        (row["repo_name"], row["command_name"]): row
        for row in rows
        if row.get("mode") == args.mode and row.get("mean_ms")
    }
    keys = sorted(set(git) | set(candidate))
    if not keys or set(git) != set(candidate):
        missing = sorted(set(git) - set(candidate))
        extra = sorted(set(candidate) - set(git))
        print(f"incomplete common-command matrix: missing={missing} extra={extra}", file=sys.stderr)
        return 2

    ratios: list[float] = []
    regressions: list[tuple[float, tuple[str, str]]] = []
    for key in keys:
        git_ms = float(git[key]["mean_ms"])
        sley_ms = float(candidate[key]["mean_ms"])
        if git_ms <= 0 or sley_ms <= 0:
            print(f"non-positive timing for {key}", file=sys.stderr)
            return 2
        ratio = sley_ms / git_ms
        ratios.append(ratio)
        if ratio > args.max_case_slower:
            regressions.append((ratio, key))

    geomean = math.exp(sum(math.log(ratio) for ratio in ratios) / len(ratios))
    print(f"common-command cases: {len(ratios)}")
    print(f"Sley/Git geometric-mean ratio: {geomean:.4f} (gate <= {args.max_geomean_ratio:.4f})")
    for ratio, (repo, command) in sorted(regressions, reverse=True):
        print(
            f"REGRESSION {repo}/{command}: Sley/Git={ratio:.4f} "
            f"(gate <= {args.max_case_slower:.4f})"
        )

    if geomean > args.max_geomean_ratio or regressions:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
