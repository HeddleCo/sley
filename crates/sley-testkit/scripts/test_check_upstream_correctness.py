from __future__ import annotations

import csv
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check_upstream_correctness.py")


class CorrectnessGateTest(unittest.TestCase):
    def run_gate(
        self,
        oracle_result: str,
        correctness: str,
        skips: int = 0,
        *,
        sley_result: str = "PASS",
        cell_vector: str = "EXACT",
    ) -> int:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            oracle = root / "oracle.csv"
            comparison = root / "comparison.csv"
            with oracle.open("w", newline="", encoding="utf-8") as handle:
                writer = csv.DictWriter(handle, fieldnames=["target", "script", "result"])
                writer.writeheader()
                writer.writerow({"target": "oracle", "script": "t0001.sh", "result": oracle_result})
            with comparison.open("w", newline="", encoding="utf-8") as handle:
                writer = csv.DictWriter(
                    handle,
                    fieldnames=[
                        "script",
                        "sley_result",
                        "cell_vector",
                        "correctness",
                        "unexpected_sley_skips",
                    ],
                )
                writer.writeheader()
                writer.writerow(
                    {
                        "script": "t0001.sh",
                        "sley_result": sley_result,
                        "cell_vector": cell_vector,
                        "correctness": correctness,
                        "unexpected_sley_skips": skips,
                    }
                )
            return subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--oracle-details",
                    str(oracle),
                    "--comparison-summary",
                    str(comparison),
                    "--expected-scripts",
                    "1",
                ],
                check=False,
            ).returncode

    def test_accepts_oracle_skip_when_sley_correctness_passes(self) -> None:
        self.assertEqual(self.run_gate("SKIP", "PASS", sley_result="SKIP"), 0)

    def test_rejects_dirty_oracle(self) -> None:
        self.assertEqual(self.run_gate("ABORT", "PASS"), 1)

    def test_rejects_failed_cells_or_unexpected_skip(self) -> None:
        self.assertEqual(self.run_gate("PASS", "FAIL"), 1)
        self.assertEqual(self.run_gate("PASS", "PASS", skips=1), 1)
        self.assertEqual(
            self.run_gate("PASS", "PASS", cell_vector="INCOMPARABLE"), 1
        )
        self.assertEqual(self.run_gate("PASS", "PASS", sley_result="FAIL"), 1)


if __name__ == "__main__":
    unittest.main()
