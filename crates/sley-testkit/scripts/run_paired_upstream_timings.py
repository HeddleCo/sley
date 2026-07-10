#!/usr/bin/env python3
"""Run alternating oracle/Sley upstream timing trials and analyze medians.

Nightly mode runs three pairs; certification mode runs five.  Each target gets
an isolated wave-runner artifact directory.  Odd trials run the oracle first
and even trials run Sley first to reduce order bias.
"""

from __future__ import annotations

import argparse
import csv
import importlib.util
import json
import os
import statistics
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Mapping, Sequence


ANALYZER_PATH = Path(__file__).with_name("analyze_upstream_timings.py")
ANALYZER_SPEC = importlib.util.spec_from_file_location(
    "sley_analyze_upstream_timings", ANALYZER_PATH
)
if ANALYZER_SPEC is None or ANALYZER_SPEC.loader is None:
    raise RuntimeError(f"cannot load analyzer: {ANALYZER_PATH}")
analyzer = importlib.util.module_from_spec(ANALYZER_SPEC)
sys.modules[ANALYZER_SPEC.name] = analyzer
ANALYZER_SPEC.loader.exec_module(analyzer)


class DriverError(RuntimeError):
    """A paired run could not produce trustworthy artifacts."""


@dataclass(frozen=True)
class ArtifactPaths:
    root: Path
    report: Path
    summary: Path
    timings: Path
    cells: Path
    details: Path
    history: Path
    stdout: Path
    stderr: Path


@dataclass(frozen=True)
class RunSpec:
    trial: int
    order: int
    target: str
    argv: tuple[str, ...]
    env: dict[str, str]
    artifacts: ArtifactPaths


@dataclass(frozen=True)
class RunRecord:
    trial: int
    order: int
    target: str
    started_at_utc: str
    wall_ms: float
    exit_code: int
    report: str
    summary: str
    timings: str
    cells: str
    details: str
    stdout: str
    stderr: str


@dataclass(frozen=True)
class TrialAnalysis:
    comparisons: tuple[analyzer.Comparison, ...]
    oracle_cells: Mapping[str, tuple[analyzer.Cell, ...]]
    sley_cells: Mapping[str, tuple[analyzer.Cell, ...]]


CONTROLLED_ENV = {
    "GIT_SRC_DIR",
    "SLEY_BIN",
    "SLEY_CELLS",
    "SLEY_COMPARISON",
    "SLEY_COMPARISON_SUMMARY",
    "SLEY_DETAILS",
    "SLEY_DEFAULT_HASH",
    "SLEY_HISTORY",
    "SLEY_ORACLE_BIN",
    "SLEY_ORACLE_CELLS",
    "SLEY_ORACLE_DETAILS",
    "SLEY_REPORT",
    "SLEY_RUN_LABEL",
    "SLEY_SUMMARY",
    "SLEY_TESTS",
    "SLEY_TEST_TARGET",
    "SLEY_TEST_TIMEOUT",
    "SLEY_TIMINGS",
    "SLEY_UPSTREAM_MANIFEST",
    "SLEY_UPSTREAM_T",
    "SLEY_UPSTREAM_WAVES",
}


def trial_order(trial: int) -> tuple[str, str]:
    if trial <= 0:
        raise ValueError("trial numbers are one-based")
    return ("oracle", "sley") if trial % 2 else ("sley", "oracle")


def trial_count(mode: str, override: int | None) -> int:
    if override is not None:
        if override <= 0:
            raise ValueError("trial count must be positive")
        return override
    return 5 if mode == "certification" else 3


def artifact_paths(output_dir: Path, trial: int, target: str) -> ArtifactPaths:
    root = output_dir / f"trial-{trial:02d}" / target
    return ArtifactPaths(
        root=root,
        report=root / "report.txt",
        summary=root / "summary.csv",
        timings=root / "timings.csv",
        cells=root / "cells.csv",
        details=root / "details.csv",
        history=root / "history.csv",
        stdout=root / "stdout.txt",
        stderr=root / "stderr.txt",
    )


def build_run_spec(
    args: argparse.Namespace, trial: int, order: int, target: str
) -> RunSpec:
    artifacts = artifact_paths(args.output_dir, trial, target)
    env = {
        "SLEY_TEST_TARGET": target,
        "SLEY_TESTS": args.tests,
        "SLEY_UPSTREAM_WAVES": str(args.waves),
        "SLEY_TEST_TIMEOUT": str(args.timeout),
        "SLEY_DEFAULT_HASH": args.hash,
        "SLEY_RUN_LABEL": f"{args.run_label}-trial-{trial:02d}-{target}",
        "SLEY_REPORT": str(artifacts.report),
        "SLEY_SUMMARY": str(artifacts.summary),
        "SLEY_TIMINGS": str(artifacts.timings),
        "SLEY_CELLS": str(artifacts.cells),
        "SLEY_DETAILS": str(artifacts.details),
        "SLEY_HISTORY": str(artifacts.history),
    }
    if args.git_src_dir:
        env["GIT_SRC_DIR"] = str(args.git_src_dir)
    else:
        env["SLEY_UPSTREAM_T"] = str(args.upstream_t)
    if args.manifest:
        env["SLEY_UPSTREAM_MANIFEST"] = str(args.manifest)
    if target == "oracle":
        env["SLEY_ORACLE_BIN"] = str(args.oracle_bin)
    else:
        env["SLEY_BIN"] = str(args.sley_bin)
    return RunSpec(
        trial=trial,
        order=order,
        target=target,
        argv=("sh", str(args.wave_runner)),
        env=env,
        artifacts=artifacts,
    )


def build_run_specs(args: argparse.Namespace) -> list[RunSpec]:
    count = trial_count(args.mode, args.trials)
    return [
        build_run_spec(args, trial, order, target)
        for trial in range(1, count + 1)
        for order, target in enumerate(trial_order(trial), start=1)
    ]


def _spec_json(spec: RunSpec) -> dict[str, object]:
    return {
        "trial": spec.trial,
        "order": spec.order,
        "target": spec.target,
        "argv": list(spec.argv),
        "env": spec.env,
        "artifacts": {key: str(value) for key, value in asdict(spec.artifacts).items()},
    }


def controlled_environment(base: Mapping[str, str], spec: RunSpec) -> dict[str, str]:
    env = {key: value for key, value in base.items() if key not in CONTROLLED_ENV}
    env.update(spec.env)
    return env


def execute_spec(spec: RunSpec) -> RunRecord:
    spec.artifacts.root.mkdir(parents=True, exist_ok=False)
    env = controlled_environment(os.environ, spec)
    started_at_utc = datetime.now(timezone.utc).isoformat()
    started = time.perf_counter_ns()
    try:
        with spec.artifacts.stdout.open("wb") as stdout, spec.artifacts.stderr.open(
            "wb"
        ) as stderr:
            completed = subprocess.run(spec.argv, env=env, stdout=stdout, stderr=stderr)
    except OSError as error:
        raise DriverError(
            f"trial {spec.trial} {spec.target}: could not start wave runner: {error}"
        ) from error
    wall_ms = (time.perf_counter_ns() - started) / 1_000_000

    required = (
        spec.artifacts.summary,
        spec.artifacts.timings,
        spec.artifacts.cells,
        spec.artifacts.details,
    )
    missing = [path.name for path in required if not path.is_file()]
    if missing:
        raise DriverError(
            f"trial {spec.trial} {spec.target}: wave runner exited {completed.returncode} "
            f"without required artifact(s): {', '.join(missing)}"
        )
    return RunRecord(
        trial=spec.trial,
        order=spec.order,
        target=spec.target,
        started_at_utc=started_at_utc,
        wall_ms=wall_ms,
        exit_code=completed.returncode,
        report=str(spec.artifacts.report),
        summary=str(spec.artifacts.summary),
        timings=str(spec.artifacts.timings),
        cells=str(spec.artifacts.cells),
        details=str(spec.artifacts.details),
        stdout=str(spec.artifacts.stdout),
        stderr=str(spec.artifacts.stderr),
    )


def analyze_trial(output_dir: Path, trial: int) -> TrialAnalysis:
    oracle_paths = artifact_paths(output_dir, trial, "oracle")
    sley_paths = artifact_paths(output_dir, trial, "sley")
    oracle = analyzer.load_run(oracle_paths.timings, oracle_paths.summary)
    sley = analyzer.load_run(sley_paths.timings, sley_paths.summary)
    oracle_cells = analyzer.load_cells(oracle_paths.cells)
    sley_cells = analyzer.load_cells(sley_paths.cells)
    comparisons = analyzer.compare_runs(oracle, sley, oracle_cells, sley_cells)
    return TrialAnalysis(tuple(comparisons), oracle_cells, sley_cells)


def _median_optional(values: Sequence[float | None]) -> float | None:
    present = [value for value in values if value is not None]
    return statistics.median(present) if present else None


def _summary_signature(row: analyzer.Comparison) -> tuple[object, ...]:
    return (
        row.oracle_result,
        row.sley_result,
        row.oracle_ok,
        row.oracle_notok,
        row.oracle_total,
        row.oracle_plan_total,
        row.sley_ok,
        row.sley_notok,
        row.sley_total,
        row.sley_plan_total,
        row.evidence,
    )


def median_comparisons(trials: Sequence[TrialAnalysis]) -> list[analyzer.Comparison]:
    if not trials:
        raise DriverError("no completed trials to analyze")
    indexed = [
        {comparison.script: comparison for comparison in trial.comparisons}
        for trial in trials
    ]
    scripts = sorted(set().union(*(set(trial) for trial in indexed)))
    medians: list[analyzer.Comparison] = []

    for script in scripts:
        rows = [trial.get(script) for trial in indexed]
        present = [row for row in rows if row is not None]
        if not present:
            continue
        first = present[0]
        oracle_ms = _median_optional([row.oracle_ms if row else None for row in rows])
        sley_ms = _median_optional([row.sley_ms if row else None for row in rows])
        if len(present) != len(trials):
            medians.append(
                replace(
                    first,
                    oracle_ms=oracle_ms,
                    sley_ms=sley_ms,
                    comparable=False,
                    evidence="trial-median",
                    reason="missing-trial-row",
                )
            )
            continue
        matching_skips = all(
            row.oracle_result == "SKIP"
            and row.sley_result == "SKIP"
            and row.reason == "both-skip"
            for row in present
        )
        if matching_skips:
            oracle_vectors = [trial.oracle_cells.get(script) for trial in trials]
            sley_vectors = [trial.sley_cells.get(script) for trial in trials]
            stable_exact_nonwork = (
                len({_summary_signature(row) for row in present}) == 1
                and all(
                    (
                        row.oracle_ok,
                        row.oracle_notok,
                        row.oracle_total,
                        row.oracle_plan_total,
                    )
                    == (
                        row.sley_ok,
                        row.sley_notok,
                        row.sley_total,
                        row.sley_plan_total,
                    )
                    for row in present
                )
                and all(vector is not None for vector in oracle_vectors + sley_vectors)
                and all(vector == oracle_vectors[0] for vector in oracle_vectors[1:])
                and all(vector == sley_vectors[0] for vector in sley_vectors[1:])
                and oracle_vectors[0] == sley_vectors[0]
            )
            medians.append(
                replace(
                    first,
                    oracle_ms=oracle_ms,
                    sley_ms=sley_ms,
                    comparable=False,
                    evidence=(
                        "stable-exact-nonwork" if stable_exact_nonwork else "trial-median"
                    ),
                    reason=(
                        "both-skip"
                        if stable_exact_nonwork
                        else "matching-skip-cell-vector-unstable"
                    ),
                )
            )
            continue
        if any(not row.comparable for row in present):
            reasons = "+".join(sorted({row.reason for row in present if row.reason}))
            medians.append(
                replace(
                    first,
                    oracle_ms=oracle_ms,
                    sley_ms=sley_ms,
                    comparable=False,
                    evidence="trial-median",
                    reason=f"trial-incomparable:{reasons}",
                )
            )
            continue
        if len({_summary_signature(row) for row in present}) != 1:
            medians.append(
                replace(
                    first,
                    oracle_ms=oracle_ms,
                    sley_ms=sley_ms,
                    comparable=False,
                    evidence="trial-median",
                    reason="summary-varies-across-trials",
                )
            )
            continue

        oracle_vectors = [trial.oracle_cells.get(script) for trial in trials]
        sley_vectors = [trial.sley_cells.get(script) for trial in trials]
        if any(vector is None for vector in oracle_vectors + sley_vectors):
            medians.append(
                replace(
                    first,
                    oracle_ms=oracle_ms,
                    sley_ms=sley_ms,
                    comparable=False,
                    evidence="trial-median",
                    reason="cell-data-missing-across-trials",
                )
            )
            continue
        if any(vector != oracle_vectors[0] for vector in oracle_vectors[1:]):
            medians.append(
                replace(
                    first,
                    oracle_ms=oracle_ms,
                    sley_ms=sley_ms,
                    comparable=False,
                    evidence="trial-median",
                    reason="oracle-cell-vector-varies-across-trials",
                )
            )
            continue
        if any(vector != sley_vectors[0] for vector in sley_vectors[1:]):
            medians.append(
                replace(
                    first,
                    oracle_ms=oracle_ms,
                    sley_ms=sley_ms,
                    comparable=False,
                    evidence="trial-median",
                    reason="sley-cell-vector-varies-across-trials",
                )
            )
            continue
        medians.append(replace(first, oracle_ms=oracle_ms, sley_ms=sley_ms))
    return medians


def _wall_work_equivalent(comparison: analyzer.Comparison) -> bool:
    return comparison.comparable or (
        comparison.oracle_result == "SKIP"
        and comparison.sley_result == "SKIP"
        and comparison.evidence == "stable-exact-nonwork"
        and comparison.reason == "both-skip"
    )


def write_run_records(path: Path, records: Sequence[RunRecord]) -> None:
    fields = list(RunRecord.__dataclass_fields__)
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        writer.writerows(asdict(record) for record in records)


def wall_time_markdown(
    records: Sequence[RunRecord],
    comparisons: Sequence[analyzer.Comparison],
    waves: int,
) -> str:
    by_trial: dict[int, dict[str, RunRecord]] = {}
    for record in records:
        by_trial.setdefault(record.trial, {})[record.target] = record
    oracle_values: list[float] = []
    sley_values: list[float] = []
    wall_label = (
        "Eight-wave wall-time gate"
        if waves == 8
        else f"{waves}-wave selected-run wall-time comparison"
    )
    lines = [
        f"## Measured {waves}-wave wall time",
        "",
        "| trial | order | Git | Sley | Git/Sley |",
        "|---:|---|---:|---:|---:|",
    ]
    for trial in sorted(by_trial):
        pair = by_trial[trial]
        if "oracle" not in pair or "sley" not in pair:
            raise DriverError(f"trial {trial}: missing wall-time record")
        oracle_ms = pair["oracle"].wall_ms
        sley_ms = pair["sley"].wall_ms
        oracle_values.append(oracle_ms)
        sley_values.append(sley_ms)
        order = " → ".join(
            record.target for record in sorted(pair.values(), key=lambda item: item.order)
        )
        lines.append(
            f"| {trial} | {order} | {oracle_ms / 1000:.2f} s | "
            f"{sley_ms / 1000:.2f} s | {oracle_ms / sley_ms:.3f}× |"
        )
    oracle_median = statistics.median(oracle_values)
    sley_median = statistics.median(sley_values)
    clean_runs = all(record.exit_code == 0 for record in records)
    equal_work = bool(comparisons) and all(
        _wall_work_equivalent(row) for row in comparisons
    ) and clean_runs
    passed = equal_work and sley_median <= oracle_median
    if equal_work:
        gate_status = "PASS" if passed else "FAIL"
        gate_note = f"{wall_label} (Sley ≤ Git): **{gate_status}**."
    else:
        incomparable = sum(
            not _wall_work_equivalent(row) for row in comparisons
        )
        failed_runs = sum(record.exit_code != 0 for record in records)
        gate_note = (
            f"{wall_label}: **NOT MEASURABLE** because "
            f"{incomparable} script(s) lack stable equal-work evidence and "
            f"{failed_runs} target run(s) exited non-zero."
        )
    lines.extend(
        [
            f"| **paired median** | alternating | **{oracle_median / 1000:.2f} s** | "
            f"**{sley_median / 1000:.2f} s** | **{oracle_median / sley_median:.3f}×** |",
            "",
            gate_note,
        ]
    )
    return "\n".join(lines) + "\n"


def wall_time_gate_passes(
    records: Sequence[RunRecord], comparisons: Sequence[analyzer.Comparison]
) -> bool:
    if (
        not comparisons
        or any(not _wall_work_equivalent(row) for row in comparisons)
        or any(record.exit_code != 0 for record in records)
    ):
        return False
    oracle = [record.wall_ms for record in records if record.target == "oracle"]
    sley = [record.wall_ms for record in records if record.target == "sley"]
    if not oracle or len(oracle) != len(sley):
        raise DriverError("cannot evaluate wall-time gate without complete pairs")
    return statistics.median(sley) <= statistics.median(oracle)


def measurable_gates_pass(
    comparisons: Sequence[analyzer.Comparison],
    records: Sequence[RunRecord],
    args: argparse.Namespace,
) -> bool:
    metrics = analyzer.calculate_metrics(comparisons, args.threshold)
    gates = analyzer.evaluate_gates(
        metrics,
        max_aggregate_ratio=args.max_aggregate_ratio,
        min_median_speedup=args.min_median_speedup,
        max_p95_ratio=args.max_p95_ratio,
        max_p99_ratio=args.max_p99_ratio,
    )
    return all(gate.passed for gate in gates) and wall_time_gate_passes(
        records, comparisons
    )


def render_paired_report(
    comparisons: Sequence[analyzer.Comparison],
    records: Sequence[RunRecord],
    args: argparse.Namespace,
) -> str:
    report = analyzer.render_markdown(
        comparisons,
        threshold=args.threshold,
        top=args.top,
        max_aggregate_ratio=args.max_aggregate_ratio,
        min_median_speedup=args.min_median_speedup,
        max_p95_ratio=args.max_p95_ratio,
        max_p99_ratio=args.max_p99_ratio,
    )
    _, body = report.split("\n", 1)
    count = trial_count(args.mode, args.trials)
    return (
        "# Paired upstream trial-median timing analysis\n\n"
        f"Mode: `{args.mode}`; paired trials: {count}; waves per target: {args.waves}. "
        "Each script duration below is the median of its paired trial durations, and a script "
        "is comparable only when every trial has stable exact cell evidence.\n\n"
        + wall_time_markdown(records, comparisons, args.waves)
        + "\n"
        + body.lstrip("\n")
    )


def validate_real_run(args: argparse.Namespace) -> None:
    required_files = (args.wave_runner, args.preflight, args.oracle_bin, args.sley_bin)
    for path in required_files:
        if not path.is_file():
            raise DriverError(f"required file does not exist: {path}")
    if args.git_src_dir and not args.git_src_dir.is_dir():
        raise DriverError(f"Git source directory does not exist: {args.git_src_dir}")
    if args.upstream_t and not args.upstream_t.is_dir():
        raise DriverError(f"upstream t directory does not exist: {args.upstream_t}")
    if args.output_dir.exists():
        raise DriverError(f"output directory already exists: {args.output_dir}")


def validate_environment(args: argparse.Namespace) -> None:
    completed = subprocess.run(
        [sys.executable, str(args.preflight)],
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise DriverError(f"upstream environment preflight failed: {detail}")


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("nightly", "certification"), default="nightly")
    parser.add_argument("--trials", type=int, help="override 3 nightly / 5 certification")
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--oracle-bin", required=True, type=Path)
    parser.add_argument("--sley-bin", required=True, type=Path)
    upstream = parser.add_mutually_exclusive_group(required=True)
    upstream.add_argument("--git-src-dir", type=Path)
    upstream.add_argument("--upstream-t", type=Path)
    parser.add_argument(
        "--wave-runner",
        type=Path,
        default=Path(__file__).with_name("run-upstream-tests-waves.sh"),
    )
    parser.add_argument(
        "--preflight",
        type=Path,
        default=Path(__file__).with_name("preflight_upstream_environment.py"),
    )
    parser.add_argument(
        "--skip-preflight",
        action="store_true",
        help="development-only escape hatch for shards that do not use local sockets",
    )
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--tests", default="curated")
    parser.add_argument("--waves", type=int, default=8)
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument("--hash", choices=("sha1", "sha256"), default="sha1")
    parser.add_argument("--threshold", type=float, default=0.05)
    parser.add_argument("--top", type=int, default=15)
    parser.add_argument("--max-aggregate-ratio", type=float, default=0.95)
    parser.add_argument("--min-median-speedup", type=float, default=1.05)
    parser.add_argument("--max-p95-ratio", type=float, default=1.0)
    parser.add_argument("--max-p99-ratio", type=float, default=1.0)
    parser.add_argument(
        "--run-label", default=datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    )
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--fail-on-gate",
        action="store_true",
        help="exit 1 if a measurable per-script or wave wall-time gate fails",
    )
    args = parser.parse_args(argv)
    try:
        trial_count(args.mode, args.trials)
    except ValueError as error:
        parser.error(str(error))
    if args.waves <= 0:
        parser.error("--waves must be positive")
    if args.timeout < 0:
        parser.error("--timeout must be non-negative")
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


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    specs = build_run_specs(args)
    if args.dry_run:
        json.dump(
            {"mode": args.mode, "runs": [_spec_json(spec) for spec in specs]},
            sys.stdout,
            indent=2,
        )
        sys.stdout.write("\n")
        return 0

    try:
        validate_real_run(args)
        if not args.skip_preflight:
            validate_environment(args)
        args.output_dir.mkdir(parents=True)
        (args.output_dir / "run-plan.json").write_text(
            json.dumps(
                {"mode": args.mode, "runs": [_spec_json(spec) for spec in specs]},
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        records: list[RunRecord] = []
        for spec in specs:
            print(
                f"trial {spec.trial}: run {spec.order}/2 {spec.target}",
                file=sys.stderr,
                flush=True,
            )
            records.append(execute_spec(spec))
            write_run_records(args.output_dir / "runs.csv", records)
        analyses = [
            analyze_trial(args.output_dir, trial)
            for trial in range(1, trial_count(args.mode, args.trials) + 1)
        ]
        comparisons = median_comparisons(analyses)
        analyzer.write_joined_csv(args.output_dir / "paired-median-comparison.csv", comparisons)
        report = render_paired_report(comparisons, records, args)
        (args.output_dir / "paired-median-analysis.md").write_text(report, encoding="utf-8")
        gates_passed = measurable_gates_pass(comparisons, records, args)
    except (DriverError, analyzer.InputError, OSError) as error:
        print(f"run-paired-upstream-timings: {error}", file=sys.stderr)
        return 2
    if args.fail_on_gate and not gates_passed:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
