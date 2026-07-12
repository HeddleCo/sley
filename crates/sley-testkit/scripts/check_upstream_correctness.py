#!/usr/bin/env python3
"""Enforce clean oracle execution and oracle-applicable Sley cell parity."""

from __future__ import annotations

import argparse
import csv
import sys
from pathlib import Path


def rows(path: Path) -> list[dict[str, str]]:
    with path.open(newline="", encoding="utf-8-sig") as handle:
        reader = csv.DictReader(handle)
        if reader.fieldnames is None:
            raise ValueError(f"{path}: missing CSV header")
        return list(reader)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--oracle-details", required=True, type=Path)
    parser.add_argument("--comparison-summary", required=True, type=Path)
    parser.add_argument("--expected-scripts", type=int)
    args = parser.parse_args()

    oracle = rows(args.oracle_details)
    comparisons = rows(args.comparison_summary)
    bad_oracle = [row for row in oracle if row.get("result") not in {"PASS", "SKIP"}]
    bad_sley = [
        row
        for row in comparisons
        if row.get("correctness") != "PASS"
        or row.get("cell_vector") != "EXACT"
        or row.get("sley_result") not in {"PASS", "SKIP"}
    ]
    unexpected_skips = [
        row for row in comparisons if int(row.get("unexpected_sley_skips") or 0) > 0
    ]
    cardinality_bad = args.expected_scripts is not None and (
        len(oracle) != args.expected_scripts or len(comparisons) != args.expected_scripts
    )

    for row in bad_oracle:
        print(f"ORACLE {row.get('script')}: {row.get('result')}", file=sys.stderr)
    for row in bad_sley:
        print(
            f"SLEY {row.get('script')}: result={row.get('sley_result')} "
            f"correctness={row.get('correctness')} "
            f"vector={row.get('cell_vector')}",
            file=sys.stderr,
        )
    for row in unexpected_skips:
        print(
            f"SLEY {row.get('script')}: unexpected skips={row.get('unexpected_sley_skips')}",
            file=sys.stderr,
        )
    if cardinality_bad:
        print(
            f"script cardinality: oracle={len(oracle)} comparison={len(comparisons)} "
            f"expected={args.expected_scripts}",
            file=sys.stderr,
        )

    if bad_oracle or bad_sley or unexpected_skips or cardinality_bad:
        return 1
    print(f"correctness gate passed for {len(comparisons)} curated scripts")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"check-upstream-correctness: {error}", file=sys.stderr)
        raise SystemExit(2)
