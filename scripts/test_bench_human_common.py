from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("bench-human-common.py")
SPEC = importlib.util.spec_from_file_location("bench_human_common", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)


class PairedTimingTests(unittest.TestCase):
    def test_pair_order_alternates(self) -> None:
        self.assertEqual(bench.paired_order(0), ("git", "sley_cli"))
        self.assertEqual(bench.paired_order(1), ("sley_cli", "git"))
        self.assertEqual(bench.paired_order(2), ("git", "sley_cli"))

    def test_pair_records_trial_order_for_both_targets(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            cwd = Path(directory)
            command = [sys.executable, "-c", "pass"]
            results = bench.timed_process_pair(
                {"git": (command, cwd), "sley_cli": (command, cwd)},
                warmup=0,
                repeat=3,
                env=os.environ.copy(),
            )

        self.assertEqual(
            [run["order"] for run in results["git"]["runs"]], [0, 1, 0]
        )
        self.assertEqual(
            [run["order"] for run in results["sley_cli"]["runs"]], [1, 0, 1]
        )

    def test_output_verification_rejects_unequal_work(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            cwd = Path(directory)
            with self.assertRaisesRegex(RuntimeError, "not byte-identical"):
                bench.verify_process_pair(
                    {
                        "git": ([sys.executable, "-c", "print('git')"], cwd),
                        "sley_cli": ([sys.executable, "-c", "print('sley')"], cwd),
                    },
                    os.environ.copy(),
                )


if __name__ == "__main__":
    unittest.main()
