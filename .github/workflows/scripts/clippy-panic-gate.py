#!/usr/bin/env python3
"""Hard-fail Clippy's panic lints on the untrusted-input library crates."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


# Every workspace member must be classified. Keeping the exclusions explicit
# makes a newly added crate fail closed instead of silently escaping the gate.
GATED_CRATES = {
    "sley-gc",
    "sley-pack",
    "sley-protocol",
    "sley-remote",
}

UNGATED_CRATES = {
    "sley",
    "sley-archive",
    "sley-bench",
    "sley-cli",
    "sley-config",
    "sley-core",
    "sley-diff-merge",
    "sley-formats",
    "sley-fsck",
    "sley-grep",
    "sley-hooks",
    "sley-i18n",
    "sley-index",
    "sley-mail",
    "sley-mmap",
    "sley-notes",
    "sley-object",
    "sley-odb",
    "sley-options",
    "sley-pathspec",
    "sley-pretty",
    "sley-procinfo",
    "sley-ref-filter",
    "sley-refs",
    "sley-rev",
    "sley-sequencer",
    "sley-strbuf-expand",
    "sley-submodule",
    "sley-testkit",
    "sley-transport",
    "sley-unpack-trees",
    "sley-worktree",
}

REPO_ROOT = Path(__file__).resolve().parents[3]


def workspace_metadata() -> dict[str, object]:
    command = [
        "cargo",
        "metadata",
        "--locked",
        "--no-deps",
        "--format-version",
        "1",
    ]
    completed = subprocess.run(
        command,
        cwd=REPO_ROOT,
        check=False,
        stdout=subprocess.PIPE,
    )
    if completed.returncode != 0:
        raise SystemExit(completed.returncode)
    return json.loads(completed.stdout)


def validate_scope(metadata: dict[str, object]) -> None:
    workspace_ids = set(metadata["workspace_members"])
    workspace_packages = {
        package["name"]: package
        for package in metadata["packages"]
        if package["id"] in workspace_ids
    }
    actual = set(workspace_packages)
    classified = GATED_CRATES | UNGATED_CRATES

    problems = []
    if overlap := GATED_CRATES & UNGATED_CRATES:
        problems.append(f"crates classified as both gated and ungated: {sorted(overlap)}")
    if unclassified := actual - classified:
        problems.append(f"unclassified workspace crates: {sorted(unclassified)}")
    if stale := classified - actual:
        problems.append(f"classified crates missing from the workspace: {sorted(stale)}")

    # The command below checks library targets. If a gated package grows a
    # binary, example, benchmark, or other production target, stop and require
    # an explicit coverage decision rather than silently skipping that target.
    for name in sorted(GATED_CRATES & actual):
        target_kinds = {
            kind
            for target in workspace_packages[name]["targets"]
            for kind in target["kind"]
        }
        unexpected_kinds = target_kinds - {"lib", "test"}
        if unexpected_kinds:
            problems.append(
                f"gated crate {name!r} has unchecked target kinds: "
                f"{sorted(unexpected_kinds)}"
            )
        if "lib" not in target_kinds:
            problems.append(f"gated crate {name!r} no longer has a library target")

    if problems:
        for problem in problems:
            print(f"clippy panic gate scope error: {problem}", file=sys.stderr)
        raise SystemExit(1)

    print("Clippy panic gate scope is complete:")
    print(f"  gated production libraries: {', '.join(sorted(GATED_CRATES))}")
    print(f"  explicitly ungated crates: {len(UNGATED_CRATES)}")


def run_clippy() -> int:
    command = [
        "cargo",
        "clippy",
        "--locked",
        *(
            argument
            for package in sorted(GATED_CRATES)
            for argument in ("--package", package)
        ),
        "--lib",
        "--no-deps",
        "--",
        "-D",
        "warnings",
        "-D",
        "clippy::unwrap_used",
        "-D",
        "clippy::expect_used",
    ]
    print("+", " ".join(command), flush=True)
    return subprocess.run(command, cwd=REPO_ROOT, check=False).returncode


def main() -> int:
    validate_scope(workspace_metadata())
    return run_clippy()


if __name__ == "__main__":
    raise SystemExit(main())
