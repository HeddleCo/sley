#!/usr/bin/env python3

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("run_paired_upstream_timings.py")
SPEC = importlib.util.spec_from_file_location("run_paired_upstream_timings", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
driver = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = driver
SPEC.loader.exec_module(driver)


class PairedDriverTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def args(self, *extra: str):
        return driver.parse_args(
            [
                "--output-dir",
                str(self.root / "results"),
                "--oracle-bin",
                str(self.root / "git"),
                "--sley-bin",
                str(self.root / "sley"),
                "--git-src-dir",
                str(self.root / "git-src"),
                "--run-label",
                "test-run",
                *extra,
            ]
        )

    def test_profiles_and_alternating_order(self) -> None:
        self.assertEqual(driver.trial_count("nightly", None), 3)
        self.assertEqual(driver.trial_count("certification", None), 5)
        self.assertEqual(driver.trial_order(1), ("oracle", "sley"))
        self.assertEqual(driver.trial_order(2), ("sley", "oracle"))
        self.assertEqual(driver.trial_order(5), ("oracle", "sley"))

    def test_command_construction_is_isolated_and_target_specific(self) -> None:
        args = self.args("--mode", "certification")
        specs = driver.build_run_specs(args)

        self.assertEqual(len(specs), 10)
        self.assertEqual(
            [(spec.trial, spec.order, spec.target) for spec in specs[:4]],
            [(1, 1, "oracle"), (1, 2, "sley"), (2, 1, "sley"), (2, 2, "oracle")],
        )
        oracle = specs[0]
        sley = specs[1]
        self.assertEqual(oracle.argv, ("sh", str(args.wave_runner)))
        self.assertEqual(oracle.env["SLEY_TEST_TARGET"], "oracle")
        self.assertEqual(oracle.env["SLEY_ORACLE_BIN"], str(args.oracle_bin))
        self.assertNotIn("SLEY_BIN", oracle.env)
        self.assertEqual(sley.env["SLEY_BIN"], str(args.sley_bin))
        self.assertNotIn("SLEY_ORACLE_BIN", sley.env)
        self.assertNotEqual(oracle.artifacts.root, sley.artifacts.root)
        self.assertIn("trial-01/oracle", str(oracle.artifacts.summary))
        self.assertEqual(oracle.env["SLEY_UPSTREAM_WAVES"], "8")
        self.assertEqual(oracle.env["SLEY_TEST_TIMEOUT"], "240")
        self.assertEqual(oracle.env["SLEY_DEFAULT_HASH"], "sha1")

        env = driver.controlled_environment(
            {
                "PATH": "/bin",
                "SLEY_ORACLE_CELLS": "/stale/cells.csv",
                "SLEY_UPSTREAM_T": "/wrong/t",
            },
            oracle,
        )
        self.assertEqual(env["PATH"], "/bin")
        self.assertNotIn("SLEY_ORACLE_CELLS", env)
        self.assertNotEqual(env.get("SLEY_UPSTREAM_T"), "/wrong/t")

    def test_dry_run_prints_plan_without_touching_output_directory(self) -> None:
        output = io.StringIO()
        argv = [
            "--output-dir",
            str(self.root / "dry-results"),
            "--oracle-bin",
            str(self.root / "git"),
            "--sley-bin",
            str(self.root / "sley"),
            "--upstream-t",
            str(self.root / "t"),
            "--run-label",
            "dry",
            "--dry-run",
        ]
        with contextlib.redirect_stdout(output):
            exit_code = driver.main(argv)

        plan = json.loads(output.getvalue())
        self.assertEqual(exit_code, 0)
        self.assertEqual(len(plan["runs"]), 6)
        self.assertEqual(
            [run["target"] for run in plan["runs"]],
            ["oracle", "sley", "sley", "oracle", "oracle", "sley"],
        )
        self.assertFalse((self.root / "dry-results").exists())

    def test_environment_preflight_failure_invalidates_timing_run(self) -> None:
        args = self.args()
        completed = mock.Mock(returncode=2, stderr="socket bind denied", stdout="")
        with mock.patch.object(driver.subprocess, "run", return_value=completed):
            with self.assertRaisesRegex(driver.DriverError, "socket bind denied"):
                driver.validate_environment(args)

    @staticmethod
    def comparison(
        script: str,
        oracle_ms: float,
        sley_ms: float,
        *,
        comparable: bool = True,
        reason: str = "",
    ):
        return driver.analyzer.Comparison(
            script=script,
            command=script,
            oracle_result="PASS",
            sley_result="PASS" if comparable else "FAIL",
            oracle_ms=oracle_ms,
            sley_ms=sley_ms,
            oracle_ok=1,
            oracle_notok=0,
            oracle_total=1,
            oracle_plan_total=1,
            sley_ok=1 if comparable else 0,
            sley_notok=0 if comparable else 1,
            sley_total=1,
            sley_plan_total=1,
            comparable=comparable,
            evidence="exact-cell-vector" if comparable else "none",
            reason=reason,
        )

    @staticmethod
    def cells(outcome: str = "pass"):
        return (driver.analyzer.Cell("1", outcome, True),)

    @staticmethod
    def skipped_comparison(script: str, oracle_ms: float, sley_ms: float):
        return driver.analyzer.Comparison(
            script=script,
            command=script,
            oracle_result="SKIP",
            sley_result="SKIP",
            oracle_ms=oracle_ms,
            sley_ms=sley_ms,
            oracle_ok=0,
            oracle_notok=0,
            oracle_total=0,
            oracle_plan_total=0,
            sley_ok=0,
            sley_notok=0,
            sley_total=0,
            sley_plan_total=0,
            comparable=False,
            evidence="none",
            reason="both-skip",
        )

    def test_trial_medians_require_every_trial_to_be_comparable(self) -> None:
        trials = [
            driver.TrialAnalysis(
                (
                    self.comparison("stable.sh", 100, 80),
                    self.comparison("early.sh", 100, 80),
                ),
                {"stable.sh": self.cells(), "early.sh": self.cells()},
                {"stable.sh": self.cells(), "early.sh": self.cells()},
            ),
            driver.TrialAnalysis(
                (
                    self.comparison("stable.sh", 120, 90),
                    self.comparison(
                        "early.sh", 100, 1, comparable=False, reason="sley-fail"
                    ),
                ),
                {"stable.sh": self.cells(), "early.sh": self.cells()},
                {"stable.sh": self.cells(), "early.sh": self.cells("fail")},
            ),
            driver.TrialAnalysis(
                (
                    self.comparison("stable.sh", 110, 85),
                    self.comparison("early.sh", 100, 75),
                ),
                {"stable.sh": self.cells(), "early.sh": self.cells()},
                {"stable.sh": self.cells(), "early.sh": self.cells()},
            ),
        ]

        rows = {row.script: row for row in driver.median_comparisons(trials)}
        self.assertTrue(rows["stable.sh"].comparable)
        self.assertEqual(rows["stable.sh"].oracle_ms, 110)
        self.assertEqual(rows["stable.sh"].sley_ms, 85)
        self.assertFalse(rows["early.sh"].comparable)
        self.assertEqual(rows["early.sh"].sley_ms, 75)
        self.assertEqual(rows["early.sh"].reason, "trial-incomparable:sley-fail")

    def test_cell_vector_variation_across_trials_is_incomparable(self) -> None:
        comparison = self.comparison("flaky.sh", 100, 90)
        trials = [
            driver.TrialAnalysis(
                (comparison,), {"flaky.sh": self.cells()}, {"flaky.sh": self.cells()}
            ),
            driver.TrialAnalysis(
                (comparison,),
                {"flaky.sh": self.cells("skip")},
                {"flaky.sh": self.cells("skip")},
            ),
        ]

        row = driver.median_comparisons(trials)[0]
        self.assertFalse(row.comparable)
        self.assertEqual(row.reason, "oracle-cell-vector-varies-across-trials")

    def test_matching_stable_skips_are_wall_equivalent_but_not_performance_rows(self) -> None:
        trials = [
            driver.TrialAnalysis(
                (self.skipped_comparison("platform.sh", 30, 25),),
                {"platform.sh": self.cells("skip")},
                {"platform.sh": self.cells("skip")},
            ),
            driver.TrialAnalysis(
                (self.skipped_comparison("platform.sh", 20, 35),),
                {"platform.sh": self.cells("skip")},
                {"platform.sh": self.cells("skip")},
            ),
            driver.TrialAnalysis(
                (self.skipped_comparison("platform.sh", 25, 30),),
                {"platform.sh": self.cells("skip")},
                {"platform.sh": self.cells("skip")},
            ),
        ]

        row = driver.median_comparisons(trials)[0]
        self.assertFalse(row.comparable)
        self.assertEqual(row.evidence, "stable-exact-nonwork")
        self.assertEqual(row.reason, "both-skip")
        self.assertTrue(driver._wall_work_equivalent(row))
        with self.assertRaisesRegex(driver.analyzer.InputError, "no comparable"):
            driver.analyzer.calculate_metrics([row], 0.05)

    @staticmethod
    def record(trial: int, order: int, target: str, wall_ms: float):
        return driver.RunRecord(
            trial=trial,
            order=order,
            target=target,
            started_at_utc="2026-07-10T00:00:00+00:00",
            wall_ms=wall_ms,
            exit_code=0,
            report="report",
            summary="summary",
            timings="timings",
            cells="cells",
            details="details",
            stdout="stdout",
            stderr="stderr",
        )

    def test_measurable_gate_includes_paired_wall_time(self) -> None:
        args = self.args()
        comparison = self.comparison("fast.sh", 200, 100)
        passing_records = [
            self.record(1, 1, "oracle", 200),
            self.record(1, 2, "sley", 150),
            self.record(2, 1, "sley", 170),
            self.record(2, 2, "oracle", 210),
            self.record(3, 1, "oracle", 220),
            self.record(3, 2, "sley", 160),
        ]
        failing_records = [
            record if record.target == "oracle" else driver.replace(record, wall_ms=300)
            for record in passing_records
        ]

        self.assertTrue(driver.measurable_gates_pass([comparison], passing_records, args))
        self.assertFalse(driver.measurable_gates_pass([comparison], failing_records, args))
        wall_report = driver.wall_time_markdown(passing_records, [comparison], 8)
        self.assertIn("oracle → sley", wall_report)
        self.assertIn("sley → oracle", wall_report)
        self.assertIn("wall-time gate (Sley ≤ Git): **PASS**", wall_report)

        incomparable = driver.replace(
            comparison, script="slow.sh", comparable=False, reason="sley-fail"
        )
        self.assertFalse(
            driver.measurable_gates_pass(
                [comparison, incomparable], passing_records, args
            )
        )
        self.assertIn(
            "wall-time gate: **NOT MEASURABLE**",
            driver.wall_time_markdown(
                passing_records, [comparison, incomparable], 8
            ),
        )

        matching_skip = driver.replace(
            self.skipped_comparison("platform.sh", 20, 20),
            evidence="stable-exact-nonwork",
        )
        self.assertTrue(
            driver.wall_time_gate_passes(
                passing_records, [comparison, matching_skip]
            )
        )
        self.assertTrue(
            driver.measurable_gates_pass(
                [comparison, matching_skip], passing_records, args
            )
        )

        shard_report = driver.wall_time_markdown(passing_records, [comparison], 1)
        self.assertIn("1-wave selected-run wall-time comparison", shard_report)
        self.assertNotIn("Eight-wave wall-time gate", shard_report)


if __name__ == "__main__":
    unittest.main()
