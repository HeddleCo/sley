#!/usr/bin/env python3
"""Compare equal-work timings from two upstream Git test-suite runs.

The wave runner records elapsed time and aggregate TAP counts per script.  Those
counts are useful, but they cannot prove that two runs executed the same TAP
cells.  This analyzer therefore uses normalized per-cell CSVs when supplied and
labels the stricter aggregate-count fallback as a proxy.

Rows which fail, time out, abort, stop before their TAP plan, or otherwise do
different work are reported separately.  They are never included in speed wins
or aggregate performance statistics.
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import statistics
import sys
from collections import Counter
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable, Mapping, Sequence, TextIO


PASS_RESULTS = {"PASS", "OK"}
CELL_ID_COLUMNS = ("number", "test_id", "cell", "test", "index")
CELL_STATUS_COLUMNS = ("status", "outcome", "result")


class InputError(ValueError):
    """The input artifacts are missing required data or contradict each other."""


@dataclass(frozen=True)
class RunRow:
    script: str
    command: str
    result: str
    elapsed_ms: float
    ok: int
    notok: int
    total: int
    plan_total: int | None


@dataclass(frozen=True)
class Cell:
    identity: str
    outcome: str
    counts_toward_total: bool = True


@dataclass(frozen=True)
class Comparison:
    script: str
    command: str
    oracle_result: str
    sley_result: str
    oracle_ms: float | None
    sley_ms: float | None
    oracle_ok: int | None
    oracle_notok: int | None
    oracle_total: int | None
    oracle_plan_total: int | None
    sley_ok: int | None
    sley_notok: int | None
    sley_total: int | None
    sley_plan_total: int | None
    comparable: bool
    evidence: str
    reason: str

    @property
    def delta_ms(self) -> float | None:
        if self.oracle_ms is None or self.sley_ms is None:
            return None
        return self.sley_ms - self.oracle_ms

    @property
    def speedup(self) -> float | None:
        if (
            self.oracle_ms is None
            or self.sley_ms is None
            or self.oracle_ms <= 0
            or self.sley_ms <= 0
        ):
            return None
        return self.oracle_ms / self.sley_ms


@dataclass(frozen=True)
class Metrics:
    count: int
    exact_count: int
    proxy_count: int
    oracle_sum_ms: float
    sley_sum_ms: float
    aggregate_speedup: float
    median_speedup: float
    geomean_speedup: float
    oracle_p95_ms: float
    sley_p95_ms: float
    oracle_p99_ms: float
    sley_p99_ms: float
    wins: int
    regressions: int
    neutral: int


@dataclass(frozen=True)
class GateResult:
    name: str
    target: str
    measured: float
    passed: bool


def _read_csv(path: Path) -> tuple[list[dict[str, str]], tuple[str, ...]]:
    try:
        with path.open(newline="", encoding="utf-8-sig") as handle:
            reader = csv.DictReader(handle)
            if reader.fieldnames is None:
                raise InputError(f"{path}: CSV has no header")
            rows = [
                {key: (value or "").strip() for key, value in row.items() if key is not None}
                for row in reader
            ]
            return rows, tuple(reader.fieldnames)
    except OSError as error:
        raise InputError(f"{path}: {error}") from error


def _require_columns(path: Path, fields: Iterable[str], required: Iterable[str]) -> None:
    available = set(fields)
    missing = sorted(set(required) - available)
    if missing:
        raise InputError(f"{path}: missing required column(s): {', '.join(missing)}")


def _parse_int(path: Path, script: str, field: str, value: str) -> int:
    if value == "":
        raise InputError(f"{path}: {script}: empty {field}")
    try:
        parsed = int(value)
    except ValueError as error:
        raise InputError(f"{path}: {script}: invalid {field}={value!r}") from error
    if parsed < 0:
        raise InputError(f"{path}: {script}: negative {field}={value!r}")
    return parsed


def _parse_optional_int(path: Path, script: str, field: str, value: str) -> int | None:
    if value == "":
        return None
    return _parse_int(path, script, field, value)


def _parse_float(path: Path, script: str, field: str, value: str) -> float:
    if value == "":
        raise InputError(f"{path}: {script}: empty {field}")
    try:
        parsed = float(value)
    except ValueError as error:
        raise InputError(f"{path}: {script}: invalid {field}={value!r}") from error
    if not math.isfinite(parsed) or parsed < 0:
        raise InputError(f"{path}: {script}: invalid {field}={value!r}")
    return parsed


def _index_rows(path: Path, rows: Iterable[dict[str, str]]) -> dict[str, dict[str, str]]:
    indexed: dict[str, dict[str, str]] = {}
    for row in rows:
        script = row.get("script", "")
        if not script:
            raise InputError(f"{path}: row has an empty script")
        if script in indexed:
            raise InputError(f"{path}: duplicate script {script!r}")
        indexed[script] = row
    return indexed


def load_run(timings_path: Path, summary_path: Path) -> dict[str, RunRow]:
    timing_rows, timing_fields = _read_csv(timings_path)
    summary_rows, summary_fields = _read_csv(summary_path)
    common = ("script", "command", "result", "ok", "notok", "total", "plan_total")
    _require_columns(timings_path, timing_fields, (*common, "elapsed_ms"))
    _require_columns(summary_path, summary_fields, common)
    timings = _index_rows(timings_path, timing_rows)
    summaries = _index_rows(summary_path, summary_rows)

    if set(timings) != set(summaries):
        only_timing = sorted(set(timings) - set(summaries))
        only_summary = sorted(set(summaries) - set(timings))
        details = []
        if only_timing:
            details.append(f"timing-only={','.join(only_timing[:5])}")
        if only_summary:
            details.append(f"summary-only={','.join(only_summary[:5])}")
        raise InputError("timing/summary script sets differ: " + "; ".join(details))

    parsed: dict[str, RunRow] = {}
    for script, timing in timings.items():
        summary = summaries[script]
        for field in common[1:]:
            if timing[field] != summary[field]:
                raise InputError(
                    f"{script}: timing/summary disagree on {field}: "
                    f"{timing[field]!r} != {summary[field]!r}"
                )
        parsed[script] = RunRow(
            script=script,
            command=summary["command"],
            result=summary["result"].upper(),
            elapsed_ms=_parse_float(timings_path, script, "elapsed_ms", timing["elapsed_ms"]),
            ok=_parse_int(summary_path, script, "ok", summary["ok"]),
            notok=_parse_int(summary_path, script, "notok", summary["notok"]),
            total=_parse_int(summary_path, script, "total", summary["total"]),
            plan_total=_parse_optional_int(
                summary_path, script, "plan_total", summary["plan_total"]
            ),
        )
    return parsed


def _first_column(fields: Sequence[str], candidates: Sequence[str]) -> str | None:
    return next((candidate for candidate in candidates if candidate in fields), None)


def _normalize_cell_outcome(status: str, directive: str) -> str:
    normalized_directive = directive.strip().lstrip("#").strip().lower().replace("-", "_")
    if normalized_directive.startswith("skip"):
        return "skip"
    if normalized_directive.startswith("todo") or normalized_directive in {
        "known_breakage",
        "known-breakage",
    }:
        return "todo"

    normalized_status = status.strip().lower().replace("_", " ").replace("-", " ")
    if normalized_status in {"ok", "pass", "passed", "success"}:
        return "pass"
    if normalized_status in {"not ok", "fail", "failed", "failure"}:
        return "fail"
    if normalized_status in {"skip", "skipped"}:
        return "skip"
    if normalized_status in {"todo", "known breakage"}:
        return "todo"
    raise InputError(f"unrecognized cell status/directive: {status!r}/{directive!r}")


def load_cells(path: Path) -> dict[str, tuple[Cell, ...]]:
    rows, fields = _read_csv(path)
    _require_columns(path, fields, ("script",))
    identity_column = _first_column(fields, CELL_ID_COLUMNS)
    status_column = _first_column(fields, CELL_STATUS_COLUMNS)
    if status_column is None:
        raise InputError(
            f"{path}: expected one cell status column: {', '.join(CELL_STATUS_COLUMNS)}"
        )
    directive_column = "directive" if "directive" in fields else None

    by_script: dict[str, list[Cell]] = {}
    ordinal_by_script: Counter[str] = Counter()
    seen: set[tuple[str, str]] = set()
    for row in rows:
        script = row.get("script", "")
        if not script:
            raise InputError(f"{path}: cell row has an empty script")
        ordinal_by_script[script] += 1
        identity = (
            row.get(identity_column, "") if identity_column else str(ordinal_by_script[script])
        )
        if not identity:
            raise InputError(f"{path}: {script}: cell has an empty identity")
        key = (script, identity)
        if key in seen:
            raise InputError(f"{path}: duplicate cell {script!r}/{identity!r}")
        seen.add(key)
        directive = row.get(directive_column, "") if directive_column else ""
        try:
            outcome = _normalize_cell_outcome(row.get(status_column, ""), directive)
        except InputError as error:
            raise InputError(f"{path}: {script}/{identity}: {error}") from error
        raw_result = row.get("raw_result", "").strip().lower().replace("-", "_")
        counts_toward_total = identity.lower() != "plan" and raw_result != "plan"
        by_script.setdefault(script, []).append(Cell(identity, outcome, counts_toward_total))
    return {script: tuple(cells) for script, cells in by_script.items()}


def _is_complete(row: RunRow) -> bool:
    return (
        row.plan_total is not None
        and row.total == row.plan_total
        and row.ok + row.notok == row.total
    )


def _comparison(
    script: str,
    oracle: RunRow | None,
    sley: RunRow | None,
    oracle_cells: Mapping[str, tuple[Cell, ...]] | None,
    sley_cells: Mapping[str, tuple[Cell, ...]] | None,
) -> Comparison:
    def make(comparable: bool, evidence: str, reason: str) -> Comparison:
        return Comparison(
            script=script,
            command=(sley.command if sley else oracle.command if oracle else ""),
            oracle_result=oracle.result if oracle else "MISSING",
            sley_result=sley.result if sley else "MISSING",
            oracle_ms=oracle.elapsed_ms if oracle else None,
            sley_ms=sley.elapsed_ms if sley else None,
            oracle_ok=oracle.ok if oracle else None,
            oracle_notok=oracle.notok if oracle else None,
            oracle_total=oracle.total if oracle else None,
            oracle_plan_total=oracle.plan_total if oracle else None,
            sley_ok=sley.ok if sley else None,
            sley_notok=sley.notok if sley else None,
            sley_total=sley.total if sley else None,
            sley_plan_total=sley.plan_total if sley else None,
            comparable=comparable,
            evidence=evidence,
            reason=reason,
        )

    if oracle is None:
        return make(False, "none", "missing-oracle-row")
    if sley is None:
        return make(False, "none", "missing-sley-row")
    oracle_pass = oracle.result in PASS_RESULTS
    sley_pass = sley.result in PASS_RESULTS
    if not oracle_pass and not sley_pass:
        if oracle.result == sley.result:
            return make(False, "none", f"both-{oracle.result.lower()}")
        for special in ("TIMEOUT", "ABORT", "SKIP"):
            if oracle.result == special:
                return make(False, "none", f"oracle-{special.lower()}")
            if sley.result == special:
                return make(False, "none", f"sley-{special.lower()}")
        return make(False, "none", "both-not-pass")
    if not oracle_pass:
        return make(False, "none", f"oracle-{oracle.result.lower()}")
    if not sley_pass:
        return make(False, "none", f"sley-{sley.result.lower()}")
    if not _is_complete(oracle) and not _is_complete(sley):
        return make(False, "none", "both-incomplete-plan")
    if not _is_complete(oracle):
        return make(False, "none", "oracle-incomplete-plan")
    if not _is_complete(sley):
        return make(False, "none", "sley-incomplete-plan")
    if oracle.elapsed_ms <= 0 or sley.elapsed_ms <= 0:
        return make(False, "none", "non-positive-elapsed-time")

    oracle_has_cells = oracle_cells is not None and script in oracle_cells
    sley_has_cells = sley_cells is not None and script in sley_cells
    if oracle_has_cells or sley_has_cells:
        if not oracle_has_cells:
            return make(False, "cell-vector", "missing-oracle-cell-data")
        if not sley_has_cells:
            return make(False, "cell-vector", "missing-sley-cell-data")
        assert oracle_cells is not None and sley_cells is not None
        if oracle_cells[script] != sley_cells[script]:
            oracle_vector = oracle_cells[script]
            sley_vector = sley_cells[script]
            identities_align = [cell.identity for cell in oracle_vector] == [
                cell.identity for cell in sley_vector
            ]
            if identities_align and any(
                oracle_cell.outcome != "skip" and sley_cell.outcome == "skip"
                for oracle_cell, sley_cell in zip(oracle_vector, sley_vector)
            ):
                return make(False, "cell-vector", "unexpected-sley-skip")
            return make(False, "cell-vector", "cell-vector-mismatch")
        oracle_assertions = sum(cell.counts_toward_total for cell in oracle_cells[script])
        sley_assertions = sum(cell.counts_toward_total for cell in sley_cells[script])
        if oracle_assertions != oracle.total or sley_assertions != sley.total:
            return make(False, "cell-vector", "cell-count-summary-mismatch")
        oracle_counts = (oracle.ok, oracle.notok, oracle.total, oracle.plan_total)
        sley_counts = (sley.ok, sley.notok, sley.total, sley.plan_total)
        if oracle_counts != sley_counts:
            return make(False, "cell-vector", "cell-summary-count-mismatch")
        return make(True, "exact-cell-vector", "")

    oracle_counts = (oracle.ok, oracle.notok, oracle.total, oracle.plan_total)
    sley_counts = (sley.ok, sley.notok, sley.total, sley.plan_total)
    if oracle_counts != sley_counts:
        return make(False, "aggregate-proxy", "aggregate-count-mismatch")
    return make(True, "aggregate-proxy", "")


def compare_runs(
    oracle: Mapping[str, RunRow],
    sley: Mapping[str, RunRow],
    oracle_cells: Mapping[str, tuple[Cell, ...]] | None = None,
    sley_cells: Mapping[str, tuple[Cell, ...]] | None = None,
) -> list[Comparison]:
    return [
        _comparison(script, oracle.get(script), sley.get(script), oracle_cells, sley_cells)
        for script in sorted(set(oracle) | set(sley))
    ]


def percentile(values: Sequence[float], percentile_value: float) -> float:
    """Linear-interpolated percentile, matching common quantile reporting."""
    if not values:
        raise ValueError("cannot calculate a percentile of an empty sequence")
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * percentile_value
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    weight = position - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def calculate_metrics(comparisons: Sequence[Comparison], threshold: float = 0.05) -> Metrics:
    comparable = [row for row in comparisons if row.comparable]
    if not comparable:
        raise InputError("no comparable equal-work rows")
    oracle_times = [row.oracle_ms for row in comparable]
    sley_times = [row.sley_ms for row in comparable]
    speedups = [row.speedup for row in comparable]
    assert all(value is not None for value in oracle_times + sley_times + speedups)
    oracle_values = [float(value) for value in oracle_times if value is not None]
    sley_values = [float(value) for value in sley_times if value is not None]
    speedup_values = [float(value) for value in speedups if value is not None]
    oracle_sum = sum(oracle_values)
    sley_sum = sum(sley_values)
    wins = sum(speedup >= 1 + threshold for speedup in speedup_values)
    regressions = sum(speedup <= 1 - threshold for speedup in speedup_values)
    return Metrics(
        count=len(comparable),
        exact_count=sum(row.evidence == "exact-cell-vector" for row in comparable),
        proxy_count=sum(row.evidence == "aggregate-proxy" for row in comparable),
        oracle_sum_ms=oracle_sum,
        sley_sum_ms=sley_sum,
        aggregate_speedup=oracle_sum / sley_sum,
        median_speedup=statistics.median(speedup_values),
        geomean_speedup=math.exp(statistics.fmean(math.log(value) for value in speedup_values)),
        oracle_p95_ms=percentile(oracle_values, 0.95),
        sley_p95_ms=percentile(sley_values, 0.95),
        oracle_p99_ms=percentile(oracle_values, 0.99),
        sley_p99_ms=percentile(sley_values, 0.99),
        wins=wins,
        regressions=regressions,
        neutral=len(comparable) - wins - regressions,
    )


def evaluate_gates(
    metrics: Metrics,
    *,
    max_aggregate_ratio: float = 0.95,
    min_median_speedup: float = 1.05,
    max_p95_ratio: float = 1.0,
    max_p99_ratio: float = 1.0,
) -> list[GateResult]:
    aggregate_ratio = metrics.sley_sum_ms / metrics.oracle_sum_ms
    p95_ratio = metrics.sley_p95_ms / metrics.oracle_p95_ms
    p99_ratio = metrics.sley_p99_ms / metrics.oracle_p99_ms
    return [
        GateResult(
            "aggregate elapsed ratio",
            f"≤ {max_aggregate_ratio:.3f}",
            aggregate_ratio,
            aggregate_ratio <= max_aggregate_ratio,
        ),
        GateResult(
            "median paired speedup",
            f"≥ {min_median_speedup:.3f}×",
            metrics.median_speedup,
            metrics.median_speedup >= min_median_speedup,
        ),
        GateResult(
            "p95 elapsed ratio",
            f"≤ {max_p95_ratio:.3f}",
            p95_ratio,
            p95_ratio <= max_p95_ratio,
        ),
        GateResult(
            "p99 elapsed ratio",
            f"≤ {max_p99_ratio:.3f}",
            p99_ratio,
            p99_ratio <= max_p99_ratio,
        ),
    ]


def _fmt_ms(value: float | None) -> str:
    if value is None:
        return "—"
    if value >= 1000:
        return f"{value / 1000:.2f} s"
    return f"{value:.0f} ms"


def _fmt_count(value: int | None) -> str:
    return "—" if value is None else str(value)


def render_markdown(
    comparisons: Sequence[Comparison],
    *,
    threshold: float = 0.05,
    top: int = 15,
    oracle_label: str = "Git",
    candidate_label: str = "Sley",
    max_aggregate_ratio: float = 0.95,
    min_median_speedup: float = 1.05,
    max_p95_ratio: float = 1.0,
    max_p99_ratio: float = 1.0,
) -> str:
    metrics = calculate_metrics(comparisons, threshold)
    gates = evaluate_gates(
        metrics,
        max_aggregate_ratio=max_aggregate_ratio,
        min_median_speedup=min_median_speedup,
        max_p95_ratio=max_p95_ratio,
        max_p99_ratio=max_p99_ratio,
    )
    comparable = [row for row in comparisons if row.comparable]
    incomparable = [row for row in comparisons if not row.comparable]
    all_oracle_sum = sum(row.oracle_ms or 0 for row in comparisons)
    all_sley_sum = sum(row.sley_ms or 0 for row in comparisons)

    lines = [
        "# Upstream equal-work timing analysis",
        "",
        (
            f"Only {metrics.count} of {len(comparisons)} paired scripts are "
            "performance-comparable: "
            f"{metrics.exact_count} have exact per-cell evidence and {metrics.proxy_count} use the "
            "strict aggregate-count proxy. Failed, timed-out, aborted, incomplete, and "
            "unequal-work rows are excluded from every speed claim."
        ),
        "",
        "The sums below are sums of per-script elapsed time captured during concurrent waves. "
        "They are not a measured serial-suite duration.",
        "",
        "## Comparable equal-work scripts",
        "",
        f"| metric | {oracle_label} | {candidate_label} | paired result |",
        "|---|---:|---:|---:|",
        (
            f"| sum of per-script elapsed | {_fmt_ms(metrics.oracle_sum_ms)} | "
            f"{_fmt_ms(metrics.sley_sum_ms)} | {metrics.aggregate_speedup:.3f}× |"
        ),
        (
            f"| median paired speedup | — | — | {metrics.median_speedup:.3f}× |"
        ),
        (
            f"| geometric-mean paired speedup | — | — | {metrics.geomean_speedup:.3f}× |"
        ),
        (
            f"| p95 script elapsed | {_fmt_ms(metrics.oracle_p95_ms)} | "
            f"{_fmt_ms(metrics.sley_p95_ms)} | — |"
        ),
        (
            f"| p99 script elapsed | {_fmt_ms(metrics.oracle_p99_ms)} | "
            f"{_fmt_ms(metrics.sley_p99_ms)} | — |"
        ),
        "",
        (
            f"At the ±{threshold * 100:.0f}% threshold: {metrics.wins} valid "
            f"{candidate_label} wins, "
            f"{metrics.regressions} regressions, and {metrics.neutral} within the band."
        ),
        "",
        "### Measurable release gates",
        "",
        "| gate | target | measured | status |",
        "|---|---:|---:|---|",
    ]
    for gate in gates:
        suffix = "×" if "speedup" in gate.name else ""
        lines.append(
            f"| {gate.name} | {gate.target} | {gate.measured:.3f}{suffix} | "
            f"{'PASS' if gate.passed else 'FAIL'} |"
        )
    lines.extend(
        [
            "",
            "Eight-wave wall time and common-command cells require their own artifacts and are not "
            "inferred from per-script CSVs.",
            "",
            "### Largest valid regressions by aggregate impact",
            "",
            f"| script | {oracle_label} | {candidate_label} | delta | speedup | evidence |",
            "|---|---:|---:|---:|---:|---|",
        ]
    )
    regressions = sorted(
        (row for row in comparable if (row.speedup or 0) <= 1 - threshold),
        key=lambda row: row.delta_ms or 0,
        reverse=True,
    )[:top]
    if regressions:
        for row in regressions:
            lines.append(
                f"| `{row.script}` | {_fmt_ms(row.oracle_ms)} | {_fmt_ms(row.sley_ms)} | "
                f"+{_fmt_ms(row.delta_ms)} | {row.speedup:.3f}× | {row.evidence} |"
            )
    else:
        lines.append("| _none_ | — | — | — | — | — |")

    lines.extend(
        [
            "",
            "### Largest valid wins by aggregate impact",
            "",
            f"| script | {oracle_label} | {candidate_label} | time saved | speedup | evidence |",
            "|---|---:|---:|---:|---:|---|",
        ]
    )
    wins = sorted(
        (row for row in comparable if (row.speedup or 0) >= 1 + threshold),
        key=lambda row: row.delta_ms or 0,
    )[:top]
    if wins:
        for row in wins:
            lines.append(
                f"| `{row.script}` | {_fmt_ms(row.oracle_ms)} | {_fmt_ms(row.sley_ms)} | "
                f"{_fmt_ms(-(row.delta_ms or 0))} | {row.speedup:.3f}× | {row.evidence} |"
            )
    else:
        lines.append("| _none_ | — | — | — | — | — |")

    reason_counts = Counter(row.reason for row in incomparable)
    lines.extend(
        [
            "",
            "## Incomparable scripts",
            "",
            (
                f"{len(incomparable)} scripts are correctness or equal-work diagnostics only. "
                "Their timings do not contribute to wins, regressions, percentiles, or speedup."
            ),
            "",
            "| reason | scripts |",
            "|---|---:|",
        ]
    )
    for reason, count in sorted(reason_counts.items(), key=lambda item: (-item[1], item[0])):
        lines.append(f"| `{reason}` | {count} |")

    lines.extend(
        [
            "",
            "### Costliest incomparable rows",
            "",
            f"| script | {oracle_label} result | {candidate_label} result | "
            f"{oracle_label} | {candidate_label} | TAP totals "
            f"({oracle_label}/{candidate_label}) | reason |",
            "|---|---|---|---:|---:|---:|---|",
        ]
    )
    for row in sorted(incomparable, key=lambda item: item.sley_ms or -1, reverse=True)[:top]:
        lines.append(
            f"| `{row.script}` | {row.oracle_result} | {row.sley_result} | "
            f"{_fmt_ms(row.oracle_ms)} | {_fmt_ms(row.sley_ms)} | "
            f"{_fmt_count(row.oracle_total)}/{_fmt_count(row.sley_total)} | `{row.reason}` |"
        )

    lines.extend(
        [
            "",
            "## All-run diagnostic totals (not equal work)",
            "",
            f"- {oracle_label}: {_fmt_ms(all_oracle_sum)} sum of per-script elapsed",
            f"- {candidate_label}: {_fmt_ms(all_sley_sum)} sum of per-script elapsed",
            "",
            "These all-run totals help identify suite cost, but differing correctness means their "
            "ratio is not a valid implementation speed comparison.",
        ]
    )
    return "\n".join(lines) + "\n"


def write_joined_csv(path: Path, comparisons: Sequence[Comparison]) -> None:
    fieldnames = [
        "script",
        "command",
        "oracle_result",
        "sley_result",
        "oracle_ms",
        "sley_ms",
        "delta_ms",
        "speedup",
        "comparable",
        "evidence",
        "reason",
        "oracle_ok",
        "oracle_notok",
        "oracle_total",
        "oracle_plan_total",
        "sley_ok",
        "sley_notok",
        "sley_total",
        "sley_plan_total",
    ]
    try:
        with path.open("w", newline="", encoding="utf-8") as handle:
            writer = csv.DictWriter(handle, fieldnames=fieldnames)
            writer.writeheader()
            for row in comparisons:
                data = _comparison_dict(row)
                writer.writerow({field: data.get(field) for field in fieldnames})
    except OSError as error:
        raise InputError(f"{path}: {error}") from error


def _comparison_dict(row: Comparison) -> dict[str, object]:
    data: dict[str, object] = asdict(row)
    data["delta_ms"] = row.delta_ms
    data["speedup"] = row.speedup
    return data


def _json_report(
    comparisons: Sequence[Comparison],
    threshold: float,
    *,
    max_aggregate_ratio: float = 0.95,
    min_median_speedup: float = 1.05,
    max_p95_ratio: float = 1.0,
    max_p99_ratio: float = 1.0,
) -> str:
    metrics = calculate_metrics(comparisons, threshold)
    gates = evaluate_gates(
        metrics,
        max_aggregate_ratio=max_aggregate_ratio,
        min_median_speedup=min_median_speedup,
        max_p95_ratio=max_p95_ratio,
        max_p99_ratio=max_p99_ratio,
    )
    return json.dumps(
        {
            "metrics": asdict(metrics),
            "gates": [asdict(gate) for gate in gates],
            "comparable": [
                _comparison_dict(row) for row in comparisons if row.comparable
            ],
            "incomparable": [
                _comparison_dict(row) for row in comparisons if not row.comparable
            ],
        },
        indent=2,
        sort_keys=True,
    ) + "\n"


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle-timings", required=True, type=Path)
    parser.add_argument("--oracle-summary", required=True, type=Path)
    parser.add_argument("--sley-timings", required=True, type=Path)
    parser.add_argument("--sley-summary", required=True, type=Path)
    parser.add_argument("--oracle-cells", type=Path)
    parser.add_argument("--sley-cells", type=Path)
    parser.add_argument("--format", choices=("markdown", "json"), default="markdown")
    parser.add_argument("--output", type=Path, help="write report here instead of stdout")
    parser.add_argument("--joined-csv", type=Path, help="also write every joined classification")
    parser.add_argument("--threshold", type=float, default=0.05)
    parser.add_argument("--top", type=int, default=15)
    parser.add_argument("--oracle-label", default="Git")
    parser.add_argument("--candidate-label", default="Sley")
    parser.add_argument("--max-aggregate-ratio", type=float, default=0.95)
    parser.add_argument("--min-median-speedup", type=float, default=1.05)
    parser.add_argument("--max-p95-ratio", type=float, default=1.0)
    parser.add_argument("--max-p99-ratio", type=float, default=1.0)
    parser.add_argument(
        "--fail-on-measurable-gate",
        action="store_true",
        help="exit 1 when an aggregate, median, p95, or p99 gate fails",
    )
    args = parser.parse_args(argv)
    if (args.oracle_cells is None) != (args.sley_cells is None):
        parser.error("--oracle-cells and --sley-cells must be supplied together")
    if not 0 <= args.threshold < 1:
        parser.error("--threshold must be in [0, 1)")
    if args.top <= 0:
        parser.error("--top must be positive")
    if min(
        args.max_aggregate_ratio,
        args.min_median_speedup,
        args.max_p95_ratio,
        args.max_p99_ratio,
    ) <= 0:
        parser.error("gate ratios and speedups must be positive")
    return args


def analyze(args: argparse.Namespace) -> tuple[str, list[Comparison]]:
    oracle = load_run(args.oracle_timings, args.oracle_summary)
    sley = load_run(args.sley_timings, args.sley_summary)
    oracle_cells = load_cells(args.oracle_cells) if args.oracle_cells else None
    sley_cells = load_cells(args.sley_cells) if args.sley_cells else None
    comparisons = compare_runs(oracle, sley, oracle_cells, sley_cells)
    if args.joined_csv:
        write_joined_csv(args.joined_csv, comparisons)
    if args.format == "json":
        report = _json_report(
            comparisons,
            args.threshold,
            max_aggregate_ratio=args.max_aggregate_ratio,
            min_median_speedup=args.min_median_speedup,
            max_p95_ratio=args.max_p95_ratio,
            max_p99_ratio=args.max_p99_ratio,
        )
    else:
        report = render_markdown(
            comparisons,
            threshold=args.threshold,
            top=args.top,
            oracle_label=args.oracle_label,
            candidate_label=args.candidate_label,
            max_aggregate_ratio=args.max_aggregate_ratio,
            min_median_speedup=args.min_median_speedup,
            max_p95_ratio=args.max_p95_ratio,
            max_p99_ratio=args.max_p99_ratio,
        )
    return report, comparisons


def run(args: argparse.Namespace) -> str:
    report, _ = analyze(args)
    return report


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        report, comparisons = analyze(args)
        if args.output:
            args.output.write_text(report, encoding="utf-8")
        else:
            sys.stdout.write(report)
        if args.fail_on_measurable_gate:
            metrics = calculate_metrics(comparisons, args.threshold)
            gates = evaluate_gates(
                metrics,
                max_aggregate_ratio=args.max_aggregate_ratio,
                min_median_speedup=args.min_median_speedup,
                max_p95_ratio=args.max_p95_ratio,
                max_p99_ratio=args.max_p99_ratio,
            )
            if not all(gate.passed for gate in gates):
                return 1
    except (InputError, OSError) as error:
        print(f"analyze-upstream-timings: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
