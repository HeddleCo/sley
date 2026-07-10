#!/usr/bin/env python3
"""Reject environments that cannot run Git's local transport/IPC tests.

Socket-denying sandboxes turn credential-cache, fsmonitor, ssh-agent, and
protocol tests into semantic-looking failures. Certification must fail before
starting either target in that environment instead of recording false gaps.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import socket
import tempfile


class PreflightError(RuntimeError):
    """The host cannot provide a facility required by the curated suite."""


def probe_tcp_loopback() -> None:
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
    except OSError as exc:
        raise PreflightError(f"loopback TCP bind/listen failed: {exc}") from exc


def probe_unix_socket(tmpdir: Path | None = None) -> None:
    if os.name == "nt" or not hasattr(socket, "AF_UNIX"):
        return
    parent = str(tmpdir) if tmpdir is not None else None
    try:
        with tempfile.TemporaryDirectory(prefix="sley-ipc-", dir=parent) as root:
            path = Path(root) / "probe.sock"
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(str(path))
                listener.listen(1)
    except OSError as exc:
        raise PreflightError(f"Unix-domain socket bind/listen failed: {exc}") from exc


def run_preflight(tmpdir: Path | None = None) -> None:
    probe_tcp_loopback()
    probe_unix_socket(tmpdir)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tmpdir",
        type=Path,
        help="directory used for the short Unix-socket probe (default: system temp)",
    )
    args = parser.parse_args()
    try:
        run_preflight(args.tmpdir)
    except PreflightError as exc:
        parser.exit(
            2,
            "invalid upstream-test environment: "
            f"{exc}\nRun the suite where local TCP and IPC listeners are permitted.\n",
        )
    print("upstream-test environment preflight: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
