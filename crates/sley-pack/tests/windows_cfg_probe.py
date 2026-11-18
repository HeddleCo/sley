"""Prove the Linux-only memory scenario is fenced off on Windows."""
from pathlib import Path
import re
import subprocess
import sys

assert sys.platform == "win32", "this negative compile probe requires Windows"
path = Path("crates/sley-pack/tests/resolved.rs")
original = path.read_bytes()
source = original.decode("utf-8").replace("\r\n", "\n")
guard = '#[cfg(target_os = "linux")]\n#[test]\nfn live_base_bound_is_shared_and_replay_stays_within_peak_memory()'
assert source.count(guard) == 1, "expected exactly one platform guard"
try:
    path.write_bytes(source.replace(guard, guard.split("\n", 1)[1]).encode("utf-8"))
    result = subprocess.run(
        ["cargo", "check", "--locked", "-p", "sley-pack", "--test", "resolved"],
        capture_output=True, text=True,
    )
    diagnostic = re.sub(r"\x1b\[[0-9;]*m", "", result.stdout + result.stderr)
    for line in diagnostic.splitlines():
        if "cannot find function" in line:
            print(line)
    assert result.returncode != 0, "missing cfg guard unexpectedly compiled on Windows"
    assert "cannot find function `memory_child`" in diagnostic, diagnostic
    assert "cannot find function `peak_bytes`" in diagnostic, diagnostic
    print(f"Windows cfg guard reverted: expected public-test compile RED (exit {result.returncode})")
finally:
    path.write_bytes(original)
print("Linux cfg guard restored before the public API GREEN test run")
