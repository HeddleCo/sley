from __future__ import annotations

import csv
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check-human-common-performance.py")


class HumanCommonGateTest(unittest.TestCase):
    def run_gate(self, rows: list[tuple[str, str, str, float]]) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temp:
            summary = Path(temp) / "timing-summary.csv"
            with summary.open("w", newline="", encoding="utf-8") as handle:
                writer = csv.writer(handle)
                writer.writerow(["repo_name", "command_name", "mode", "mean_ms"])
                writer.writerows(rows)
            return subprocess.run(
                [sys.executable, str(SCRIPT), str(summary)],
                text=True,
                capture_output=True,
                check=False,
            )

    def test_passes_when_every_case_and_geomean_clear_gates(self) -> None:
        result = self.run_gate(
            [
                ("small", "status", "git", 100),
                ("small", "status", "sley_cli", 90),
                ("large", "log", "git", 200),
                ("large", "log", "sley_cli", 180),
            ]
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_fails_single_case_regression_even_if_geomean_is_fast(self) -> None:
        result = self.run_gate(
            [
                ("small", "status", "git", 100),
                ("small", "status", "sley_cli", 106),
                ("large", "log", "git", 200),
                ("large", "log", "sley_cli", 100),
            ]
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("REGRESSION small/status", result.stdout)

    def test_rejects_incomplete_pair_matrix(self) -> None:
        result = self.run_gate([("small", "status", "git", 100)])
        self.assertEqual(result.returncode, 2)
        self.assertIn("incomplete", result.stderr)


if __name__ == "__main__":
    unittest.main()
