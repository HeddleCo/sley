#!/usr/bin/env python3

from __future__ import annotations

import csv
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("analyze_upstream_timings.py")
SPEC = importlib.util.spec_from_file_location("analyze_upstream_timings", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
analyzer = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = analyzer
SPEC.loader.exec_module(analyzer)


class AnalyzerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def write_csv(self, name: str, fields: list[str], rows: list[dict[str, object]]) -> Path:
        path = self.root / name
        with path.open("w", newline="", encoding="utf-8") as handle:
            writer = csv.DictWriter(handle, fieldnames=fields)
            writer.writeheader()
            writer.writerows(rows)
        return path

    def write_run(self, prefix: str, rows: list[dict[str, object]]) -> tuple[Path, Path]:
        common = ["script", "command", "result", "ok", "notok", "total", "plan_total"]
        timings = self.write_csv(
            f"{prefix}-timings.csv",
            ["label", *common, "elapsed_ms"],
            [{"label": prefix, **row} for row in rows],
        )
        summaries = self.write_csv(
            f"{prefix}-summary.csv",
            common,
            [{field: row[field] for field in common} for row in rows],
        )
        return timings, summaries

    def load_pair(
        self, oracle_rows: list[dict[str, object]], sley_rows: list[dict[str, object]]
    ) -> tuple[dict[str, object], dict[str, object]]:
        oracle_timing, oracle_summary = self.write_run("oracle", oracle_rows)
        sley_timing, sley_summary = self.write_run("sley", sley_rows)
        return (
            analyzer.load_run(oracle_timing, oracle_summary),
            analyzer.load_run(sley_timing, sley_summary),
        )

    @staticmethod
    def row(
        script: str,
        elapsed_ms: int,
        *,
        result: str = "PASS",
        ok: int = 2,
        notok: int = 0,
        total: int = 2,
        plan_total: int = 2,
    ) -> dict[str, object]:
        return {
            "script": script,
            "command": script.removesuffix(".sh"),
            "result": result,
            "elapsed_ms": elapsed_ms,
            "ok": ok,
            "notok": notok,
            "total": total,
            "plan_total": plan_total,
        }

    def test_aggregate_fallback_is_strict_and_excludes_early_failures(self) -> None:
        oracle, sley = self.load_pair(
            [
                self.row("comparable.sh", 200),
                self.row("early.sh", 500),
                self.row("counts.sh", 100, ok=3, total=3, plan_total=3),
                self.row("incomplete.sh", 100, ok=1, total=1, plan_total=2),
                self.row("timeout.sh", 100),
            ],
            [
                self.row("comparable.sh", 100),
                self.row("early.sh", 1, result="FAIL", ok=0, notok=1),
                self.row("counts.sh", 50),
                self.row("incomplete.sh", 50, ok=1, total=1, plan_total=2),
                self.row("timeout.sh", 1, result="TIMEOUT", ok=0, total=0, plan_total=""),
            ],
        )
        comparisons = analyzer.compare_runs(oracle, sley)
        by_script = {row.script: row for row in comparisons}

        self.assertTrue(by_script["comparable.sh"].comparable)
        self.assertEqual(by_script["comparable.sh"].evidence, "aggregate-proxy")
        self.assertEqual(by_script["early.sh"].reason, "sley-fail")
        self.assertEqual(by_script["counts.sh"].reason, "aggregate-count-mismatch")
        self.assertEqual(by_script["incomplete.sh"].reason, "both-incomplete-plan")
        self.assertEqual(by_script["timeout.sh"].reason, "sley-timeout")

        metrics = analyzer.calculate_metrics(comparisons)
        self.assertEqual(metrics.count, 1)
        self.assertEqual(metrics.wins, 1)
        report = analyzer.render_markdown(comparisons)
        valid_wins = report.split("### Largest valid wins", 1)[1].split(
            "## Incomparable scripts", 1
        )[0]
        self.assertIn("comparable.sh", valid_wins)
        self.assertNotIn("early.sh", valid_wins)
        self.assertIn("not a measured serial-suite duration", report)

    def test_exact_cells_override_aggregate_proxy_and_detect_mismatch(self) -> None:
        oracle, sley = self.load_pair(
            [self.row("exact.sh", 200), self.row("different.sh", 200)],
            [self.row("exact.sh", 100), self.row("different.sh", 100)],
        )
        fields = ["script", "number", "status", "directive"]
        oracle_cell_path = self.write_csv(
            "oracle-cells.csv",
            fields,
            [
                {"script": "exact.sh", "number": 1, "status": "ok", "directive": ""},
                {"script": "exact.sh", "number": 2, "status": "not ok", "directive": "TODO"},
                {"script": "different.sh", "number": 1, "status": "ok", "directive": ""},
                {"script": "different.sh", "number": 2, "status": "ok", "directive": "SKIP"},
            ],
        )
        sley_cell_path = self.write_csv(
            "sley-cells.csv",
            fields,
            [
                {"script": "exact.sh", "number": 1, "status": "pass", "directive": ""},
                {"script": "exact.sh", "number": 2, "status": "failed", "directive": "todo"},
                {"script": "different.sh", "number": 1, "status": "ok", "directive": ""},
                {"script": "different.sh", "number": 2, "status": "ok", "directive": ""},
            ],
        )
        comparisons = analyzer.compare_runs(
            oracle,
            sley,
            analyzer.load_cells(oracle_cell_path),
            analyzer.load_cells(sley_cell_path),
        )
        by_script = {row.script: row for row in comparisons}

        self.assertTrue(by_script["exact.sh"].comparable)
        self.assertEqual(by_script["exact.sh"].evidence, "exact-cell-vector")
        self.assertFalse(by_script["different.sh"].comparable)
        self.assertEqual(by_script["different.sh"].reason, "cell-vector-mismatch")

    def test_metrics_are_paired_and_geometric(self) -> None:
        oracle, sley = self.load_pair(
            [self.row("a.sh", 200), self.row("b.sh", 100), self.row("c.sh", 400)],
            [self.row("a.sh", 100), self.row("b.sh", 200), self.row("c.sh", 200)],
        )
        metrics = analyzer.calculate_metrics(analyzer.compare_runs(oracle, sley))

        self.assertEqual(metrics.count, 3)
        self.assertAlmostEqual(metrics.aggregate_speedup, 1.4)
        self.assertAlmostEqual(metrics.median_speedup, 2.0)
        self.assertAlmostEqual(metrics.geomean_speedup, (2 * 0.5 * 2) ** (1 / 3))
        self.assertEqual((metrics.wins, metrics.regressions, metrics.neutral), (2, 1, 0))

    def test_cli_json_and_joined_csv(self) -> None:
        oracle_timing, oracle_summary = self.write_run("oracle", [self.row("a.sh", 200)])
        sley_timing, sley_summary = self.write_run("sley", [self.row("a.sh", 100)])
        joined = self.root / "joined.csv"
        args = analyzer.parse_args(
            [
                "--oracle-timings",
                str(oracle_timing),
                "--oracle-summary",
                str(oracle_summary),
                "--sley-timings",
                str(sley_timing),
                "--sley-summary",
                str(sley_summary),
                "--format",
                "json",
                "--joined-csv",
                str(joined),
            ]
        )

        report = json.loads(analyzer.run(args))
        self.assertEqual(report["metrics"]["count"], 1)
        self.assertEqual(report["metrics"]["aggregate_speedup"], 2.0)
        self.assertEqual(report["comparable"][0]["speedup"], 2.0)
        self.assertEqual(report["comparable"][0]["delta_ms"], -100.0)
        self.assertTrue(all(gate["passed"] for gate in report["gates"]))
        with joined.open(newline="", encoding="utf-8") as handle:
            joined_rows = list(csv.DictReader(handle))
        self.assertEqual(joined_rows[0]["comparable"], "True")
        self.assertEqual(joined_rows[0]["evidence"], "aggregate-proxy")

    def test_rejects_timing_summary_disagreement(self) -> None:
        timings, summaries = self.write_run("bad", [self.row("a.sh", 100)])
        with summaries.open(newline="", encoding="utf-8") as handle:
            rows = list(csv.DictReader(handle))
        rows[0]["ok"] = "1"
        self.write_csv("bad-summary.csv", list(rows[0]), rows)

        with self.assertRaisesRegex(analyzer.InputError, "disagree on ok"):
            analyzer.load_run(timings, summaries)

    def test_empty_plan_from_aborted_run_is_incomparable_not_invalid_input(self) -> None:
        oracle, sley = self.load_pair(
            [self.row("aborted.sh", 30, result="FAIL", ok=0, total=0, plan_total="")],
            [self.row("aborted.sh", 10, result="FAIL", ok=0, total=0, plan_total="")],
        )

        comparison = analyzer.compare_runs(oracle, sley)[0]
        self.assertFalse(comparison.comparable)
        self.assertEqual(comparison.reason, "both-fail")

    def test_plan_pseudocell_does_not_count_as_an_assertion(self) -> None:
        oracle, sley = self.load_pair(
            [self.row("skip-all.sh", 30, ok=0, total=0, plan_total=0)],
            [self.row("skip-all.sh", 20, ok=0, total=0, plan_total=0)],
        )
        fields = ["target", "script", "cell", "status", "raw_result", "directive"]
        oracle_cells_path = self.write_csv(
            "oracle-plan-cells.csv",
            fields,
            [
                {
                    "target": "oracle",
                    "script": "skip-all.sh",
                    "cell": "plan",
                    "status": "SKIP",
                    "raw_result": "plan",
                    "directive": "SKIP",
                }
            ],
        )
        sley_cells_path = self.write_csv(
            "sley-plan-cells.csv",
            fields,
            [
                {
                    "target": "sley",
                    "script": "skip-all.sh",
                    "cell": "plan",
                    "status": "SKIP",
                    "raw_result": "plan",
                    "directive": "SKIP",
                }
            ],
        )

        comparison = analyzer.compare_runs(
            oracle,
            sley,
            analyzer.load_cells(oracle_cells_path),
            analyzer.load_cells(sley_cells_path),
        )[0]
        self.assertTrue(comparison.comparable)
        self.assertEqual(comparison.evidence, "exact-cell-vector")

    def test_unexpected_sley_skip_gets_a_specific_reason(self) -> None:
        oracle, sley = self.load_pair(
            [self.row("skip.sh", 30, ok=1, total=1, plan_total=1)],
            [self.row("skip.sh", 20, ok=1, total=1, plan_total=1)],
        )
        fields = ["script", "cell", "status", "raw_result", "directive"]
        oracle_cells_path = self.write_csv(
            "oracle-skip-cells.csv",
            fields,
            [
                {
                    "script": "skip.sh",
                    "cell": "1",
                    "status": "PASS",
                    "raw_result": "ok",
                    "directive": "",
                }
            ],
        )
        sley_cells_path = self.write_csv(
            "sley-skip-cells.csv",
            fields,
            [
                {
                    "script": "skip.sh",
                    "cell": "1",
                    "status": "SKIP",
                    "raw_result": "ok",
                    "directive": "SKIP",
                }
            ],
        )

        comparison = analyzer.compare_runs(
            oracle,
            sley,
            analyzer.load_cells(oracle_cells_path),
            analyzer.load_cells(sley_cells_path),
        )[0]
        self.assertFalse(comparison.comparable)
        self.assertEqual(comparison.reason, "unexpected-sley-skip")

    def test_cli_can_fail_on_a_measurable_release_gate(self) -> None:
        oracle_timing, oracle_summary = self.write_run("oracle", [self.row("a.sh", 200)])
        sley_timing, sley_summary = self.write_run("sley", [self.row("a.sh", 300)])
        output = self.root / "gate-report.md"

        exit_code = analyzer.main(
            [
                "--oracle-timings",
                str(oracle_timing),
                "--oracle-summary",
                str(oracle_summary),
                "--sley-timings",
                str(sley_timing),
                "--sley-summary",
                str(sley_summary),
                "--output",
                str(output),
                "--fail-on-measurable-gate",
            ]
        )

        self.assertEqual(exit_code, 1)
        self.assertIn("| aggregate elapsed ratio | ≤ 0.950 | 1.500 | FAIL |", output.read_text())


if __name__ == "__main__":
    unittest.main()
