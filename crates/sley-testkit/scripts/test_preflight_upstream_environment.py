#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("preflight_upstream_environment.py")
SPEC = importlib.util.spec_from_file_location("preflight_upstream_environment", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
preflight = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(preflight)


class PreflightTests(unittest.TestCase):
    def test_runs_both_required_socket_probes(self) -> None:
        with (
            mock.patch.object(preflight, "probe_tcp_loopback") as tcp,
            mock.patch.object(preflight, "probe_unix_socket") as unix,
            tempfile.TemporaryDirectory() as root,
        ):
            tmpdir = Path(root)
            preflight.run_preflight(tmpdir)
        tcp.assert_called_once_with()
        unix.assert_called_once_with(tmpdir)

    def test_probe_failure_invalidates_the_environment(self) -> None:
        failure = preflight.PreflightError("Operation not permitted")
        with mock.patch.object(preflight, "probe_tcp_loopback", side_effect=failure):
            with self.assertRaisesRegex(preflight.PreflightError, "Operation not permitted"):
                preflight.run_preflight()


if __name__ == "__main__":
    unittest.main()
