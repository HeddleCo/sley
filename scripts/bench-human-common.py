#!/usr/bin/env python3
"""Benchmark common human-facing git commands over real public repositories.

The matrix compares:

* git: Homebrew/system git as the baseline
* sley_cli: release `sley` process, including process startup
* sley_harness: direct in-process harness calls for matching Sley APIs

By default repositories are cached below the platform temporary directory and
results are persisted under bench-results/human-common/<timestamp>.

Git and the Sley CLI are admitted to timing only after their exit status,
stdout, and stderr are byte-identical for the case. Their warmups and measured
runs then alternate which target executes first on every repetition.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


REPOS = [
    {
        "size": "sm",
        "name": "walkdir",
        "url": "https://github.com/BurntSushi/walkdir.git",
    },
    {
        "size": "md",
        "name": "ripgrep",
        "url": "https://github.com/BurntSushi/ripgrep.git",
    },
    {
        "size": "lg",
        "name": "git",
        "url": "https://github.com/git/git.git",
    },
]

READ_COMMANDS = [
    {
        "name": "status_short",
        "kind": "read",
        "args": ["status", "--short"],
    },
    {
        "name": "log_oneline_100",
        "kind": "read",
        "args": ["log", "--oneline", "-100"],
    },
    {
        "name": "branch_list",
        "kind": "read",
        "args": ["branch", "--list"],
    },
    {
        "name": "tag_list",
        "kind": "read",
        "args": ["tag", "--list"],
    },
    {
        "name": "rev_parse_short_head",
        "kind": "read",
        "args": ["rev-parse", "--short", "HEAD"],
    },
]

WRITE_COMMANDS = [
    {
        "name": "branch_force_write",
        "kind": "write",
        "args": ["branch", "-f", "sley-bench-write", "HEAD"],
    },
    {
        "name": "tag_force_write",
        "kind": "write",
        "args": ["tag", "-f", "sley-bench-write", "HEAD"],
    },
]

# Keep this matrix to commands implemented by git, sley_cli, and sley_harness so
# every row has timing and memory numbers for all three modes.
COMMANDS = READ_COMMANDS + WRITE_COMMANDS

BENCH_ENV = {
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_PAGER": "cat",
    "PAGER": "cat",
    "LC_ALL": "C",
    "LANG": "C",
    "NO_COLOR": "1",
    "GIT_COMMITTER_NAME": "Sley Bench",
    "GIT_COMMITTER_EMAIL": "sley-bench@example.invalid",
    "GIT_COMMITTER_DATE": "@1700000000 +0000",
}


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def default_git_bin() -> Path:
    configured = os.environ.get("SLEY_TEST_GIT") or os.environ.get("SLEY_BENCH_GIT")
    return Path(configured or shutil.which("git") or "git")


def run(
    argv: list[str],
    cwd: Path | None = None,
    *,
    env: dict[str, str] | None = None,
    stdout: Any = subprocess.PIPE,
    stderr: Any = subprocess.PIPE,
    check: bool = True,
) -> subprocess.CompletedProcess[bytes]:
    proc = subprocess.run(
        argv,
        cwd=str(cwd) if cwd else None,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=stdout,
        stderr=stderr,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"{' '.join(argv)} failed with {proc.returncode}\n"
            f"stdout:\n{decode(proc.stdout)}\n"
            f"stderr:\n{decode(proc.stderr)}"
        )
    return proc


def decode(data: bytes | None) -> str:
    if data is None:
        return ""
    return data.decode("utf-8", errors="replace")


def merged_env() -> dict[str, str]:
    env = os.environ.copy()
    env.update(BENCH_ENV)
    return env


def build_binaries(root: Path) -> tuple[Path, Path]:
    run(["cargo", "build", "-p", "sley-cli", "--bin", "sley", "--release"], cwd=root)
    run(
        [
            "cargo",
            "build",
            "-p",
            "sley-bench",
            "--bin",
            "sley-human-harness",
            "--release",
        ],
        cwd=root,
    )
    return root / "target/release/sley", root / "target/release/sley-human-harness"


def ensure_repos(
    cache_dir: Path,
    update: bool,
    sizes: set[str] | None,
    git_bin: Path,
) -> list[dict[str, Any]]:
    cache_dir.mkdir(parents=True, exist_ok=True)
    repos = []
    for spec in REPOS:
        if sizes and spec["size"] not in sizes:
            continue
        path = cache_dir / f"{spec['size']}-{spec['name']}"
        if path.exists() and not is_git_repo(path, git_bin):
            shutil.rmtree(path)
        if not path.exists():
            run([str(git_bin), "clone", spec["url"], str(path)], stdout=None, stderr=None)
        elif update:
            run(
                [str(git_bin), "fetch", "--tags", "--prune"],
                cwd=path,
                stdout=None,
                stderr=None,
            )

        head = decode(run([str(git_bin), "rev-parse", "HEAD"], cwd=path).stdout).strip()
        branch = decode(
            run([str(git_bin), "rev-parse", "--abbrev-ref", "HEAD"], cwd=path).stdout
        ).strip()
        commit_count = int(
            decode(
                run([str(git_bin), "rev-list", "--count", "HEAD"], cwd=path).stdout
            ).strip()
        )
        tracked_files = int(
            decode(run([str(git_bin), "ls-files"], cwd=path).stdout).count("\n")
        )
        has_parent = (
            run(
                [str(git_bin), "rev-parse", "--verify", "--quiet", "HEAD~1"],
                cwd=path,
                check=False,
            ).returncode
            == 0
        )
        repos.append(
            {
                **spec,
                "path": str(path),
                "head": head,
                "branch": branch,
                "commit_count": commit_count,
                "tracked_files": tracked_files,
                "has_parent": has_parent,
            }
        )
    return repos


def is_git_repo(path: Path, git_bin: Path) -> bool:
    return (
        run(
            [str(git_bin), "-C", str(path), "rev-parse", "--git-dir"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )


def command_available(repo: dict[str, Any], command: dict[str, Any]) -> bool:
    return command.get("requires") != "parent" or bool(repo["has_parent"])


def command_label(command: dict[str, Any]) -> str:
    return " ".join(command["args"])


def command_kind(command: dict[str, Any]) -> str:
    return str(command.get("kind", "read"))


def is_write_command(command: dict[str, Any]) -> bool:
    return command_kind(command) == "write"


def process_argv(
    mode: str,
    command: dict[str, Any],
    repo_path: Path,
    sley_bin: Path,
    git_bin: Path,
) -> list[str]:
    if mode == "git":
        return [str(git_bin), *command["args"]]
    if mode == "sley_cli":
        return [str(sley_bin), "-C", str(repo_path), *command["args"]]
    raise ValueError(f"unsupported process mode {mode}")


def case_repo_path(
    source_repo: Path,
    repo: dict[str, Any],
    command: dict[str, Any],
    mode: str,
    phase: str,
    scratch_root: Path,
    git_bin: Path,
    index: int | None = None,
) -> Path:
    if not is_write_command(command):
        return source_repo
    return prepare_write_repo(
        source_repo,
        repo,
        command,
        mode,
        phase,
        scratch_root,
        git_bin,
        index,
    )


def prepare_write_repo(
    source_repo: Path,
    repo: dict[str, Any],
    command: dict[str, Any],
    mode: str,
    phase: str,
    scratch_root: Path,
    git_bin: Path,
    index: int | None,
) -> Path:
    parts = [phase, repo["size"], repo["name"], mode, command["name"]]
    if index is not None:
        parts.append(str(index))
    dest = scratch_root / "-".join(safe_component(str(part)) for part in parts)
    run(
        [
            str(git_bin),
            "clone",
            "--local",
            "--no-checkout",
            "--quiet",
            str(source_repo),
            str(dest),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    return dest


def safe_component(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in {"-", "_"} else "-" for ch in value)


def paired_order(index: int) -> tuple[str, str]:
    """Alternate which process runs first to reduce cache/order bias."""
    return ("git", "sley_cli") if index % 2 == 0 else ("sley_cli", "git")


def timed_process_pair(
    cases: dict[str, tuple[list[str], Path]],
    warmup: int,
    repeat: int,
    env: dict[str, str],
) -> dict[str, dict[str, Any]]:
    """Time a Git/Sley pair in alternating order for every warmup and trial."""
    results: dict[str, list[dict[str, Any]]] = {"git": [], "sley_cli": []}
    for index in range(warmup):
        for mode in paired_order(index):
            argv, cwd = cases[mode]
            proc = run(
                argv,
                cwd=cwd,
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                check=False,
            )
            if proc.returncode != 0:
                return {
                    mode: error_record(proc),
                    paired_mode(mode): {
                        "error": {
                            "returncode": -1,
                            "stderr": f"paired warmup failed in {mode}",
                            "stdout": "",
                        }
                    },
                }

    for index in range(repeat):
        for mode in paired_order(index):
            argv, cwd = cases[mode]
            start = time.perf_counter_ns()
            proc = run(
                argv,
                cwd=cwd,
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                check=False,
            )
            elapsed = time.perf_counter_ns() - start
            if proc.returncode != 0:
                failed = error_record(proc)
                failed["failed_run"] = index
                return {
                    mode: failed,
                    paired_mode(mode): {
                        "error": {
                            "returncode": -1,
                            "stderr": f"paired timing failed in {mode}",
                            "stdout": "",
                        }
                    },
                }
            results[mode].append(
                {"index": index, "order": paired_order(index).index(mode), "ns": elapsed}
            )
    return {mode: {"runs": runs} for mode, runs in results.items()}


def paired_mode(mode: str) -> str:
    return "sley_cli" if mode == "git" else "git"


def verify_process_pair(
    cases: dict[str, tuple[list[str], Path]], env: dict[str, str]
) -> None:
    """Require byte-identical command output before admitting a timed case."""
    outputs: dict[str, subprocess.CompletedProcess[bytes]] = {}
    for mode in ("git", "sley_cli"):
        argv, cwd = cases[mode]
        outputs[mode] = run(argv, cwd=cwd, env=env, check=False)
    git = outputs["git"]
    sley = outputs["sley_cli"]
    if (
        git.returncode != sley.returncode
        or git.stdout != sley.stdout
        or git.stderr != sley.stderr
    ):
        raise RuntimeError(
            "common-command case is not byte-identical before timing\n"
            f"git: rc={git.returncode} stdout={decode(git.stdout)!r} "
            f"stderr={decode(git.stderr)!r}\n"
            f"sley: rc={sley.returncode} stdout={decode(sley.stdout)!r} "
            f"stderr={decode(sley.stderr)!r}"
        )


def timed_harness(
    harness_bin: Path,
    repo: Path,
    command: dict[str, Any],
    warmup: int,
    repeat: int,
    out_dir: Path,
    env: dict[str, str],
) -> dict[str, Any]:
    out = out_dir / f"harness-{repo.name}-{command['name']}.json"
    proc = run(
        [
            str(harness_bin),
            "--repo",
            str(repo),
            "--warmup",
            str(warmup),
            "--repeat",
            str(repeat),
            "--out",
            str(out),
            "--",
            *command["args"],
        ],
        cwd=repo,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
    )
    if proc.returncode != 0:
        return error_record(proc)
    with out.open("r", encoding="utf-8") as handle:
        body = json.load(handle)
    return {"runs": body["runs"], "detail_file": str(out)}


def error_record(proc: subprocess.CompletedProcess[bytes]) -> dict[str, Any]:
    return {
        "error": {
            "returncode": proc.returncode,
            "stderr": decode(proc.stderr)[-4000:],
            "stdout": decode(proc.stdout)[-1000:],
        }
    }


def summarize_ns(runs: list[dict[str, Any]]) -> dict[str, float]:
    values = [float(run["ns"]) / 1_000_000.0 for run in runs]
    return {
        "min_ms": min(values),
        "max_ms": max(values),
        "mean_ms": statistics.fmean(values),
        "median_ms": statistics.median(values),
        "stdev_ms": statistics.stdev(values) if len(values) > 1 else 0.0,
    }


def run_timing_matrix(
    repos: list[dict[str, Any]],
    sley_bin: Path,
    harness_bin: Path,
    git_bin: Path,
    out_dir: Path,
    scratch_root: Path,
    warmup: int,
    repeat: int,
) -> list[dict[str, Any]]:
    env = merged_env()
    timing_dir = out_dir / "harness-details"
    timing_dir.mkdir(parents=True, exist_ok=True)
    records = []
    for repo in repos:
        repo_path = Path(repo["path"])
        for command in COMMANDS:
            if not command_available(repo, command):
                records.append(skip_record(repo, command, "missing HEAD~1"))
                continue
            cases: dict[str, tuple[list[str], Path]] = {}
            for mode in ["git", "sley_cli"]:
                bench_repo = case_repo_path(
                    repo_path, repo, command, mode, "timing", scratch_root, git_bin
                )
                argv = process_argv(mode, command, bench_repo, sley_bin, git_bin)
                cases[mode] = (argv, bench_repo)

            print(
                f"paired timing {repo['size']} {repo['name']} "
                f"git/sley_cli {command['name']}"
            )
            verify_process_pair(cases, env)
            paired_results = timed_process_pair(cases, warmup, repeat, env)
            for mode in ["git", "sley_cli"]:
                result = paired_results[mode]
                records.append(make_timing_record(repo, command, mode, result))

            print(f"timing {repo['size']} {repo['name']} sley_harness {command['name']}")
            bench_repo = case_repo_path(
                repo_path,
                repo,
                command,
                "sley_harness",
                "timing",
                scratch_root,
                git_bin,
            )
            result = timed_harness(
                harness_bin, bench_repo, command, warmup, repeat, timing_dir, env
            )
            records.append(make_timing_record(repo, command, "sley_harness", result))
    add_speedups(records)
    return records


def skip_record(repo: dict[str, Any], command: dict[str, Any], reason: str) -> dict[str, Any]:
    return {
        "repo_size": repo["size"],
        "repo_name": repo["name"],
        "repo_head": repo["head"],
        "command_name": command["name"],
        "command_kind": command_kind(command),
        "command": command_label(command),
        "mode": "all",
        "skipped": reason,
    }


def make_timing_record(
    repo: dict[str, Any],
    command: dict[str, Any],
    mode: str,
    result: dict[str, Any],
) -> dict[str, Any]:
    record = {
        "repo_size": repo["size"],
        "repo_name": repo["name"],
        "repo_head": repo["head"],
        "command_name": command["name"],
        "command_kind": command_kind(command),
        "command": command_label(command),
        "mode": mode,
    }
    record.update(result)
    if "runs" in result:
        record.update(summarize_ns(result["runs"]))
    return record


def add_speedups(records: list[dict[str, Any]]) -> None:
    git_means = {}
    for record in records:
        if record.get("mode") == "git" and "mean_ms" in record:
            git_means[(record["repo_name"], record["command_name"])] = record["mean_ms"]
    for record in records:
        key = (record.get("repo_name"), record.get("command_name"))
        git_mean = git_means.get(key)
        if git_mean and "mean_ms" in record:
            record["speedup_vs_git"] = git_mean / record["mean_ms"]


def run_memory_matrix(
    repos: list[dict[str, Any]],
    sley_bin: Path,
    harness_bin: Path,
    git_bin: Path,
    out_dir: Path,
    scratch_root: Path,
    runs_per_case: int,
) -> list[dict[str, Any]]:
    time_bin = shutil.which("time")
    if not Path("/usr/bin/time").exists() and not time_bin:
        raise RuntimeError("time command not found")
    time_cmd = "/usr/bin/time" if Path("/usr/bin/time").exists() else time_bin
    assert time_cmd is not None

    env = merged_env()
    mem_dir = out_dir / "memory-harness-details"
    mem_dir.mkdir(parents=True, exist_ok=True)
    records = []
    for repo in repos:
        repo_path = Path(repo["path"])
        for command in COMMANDS:
            if not command_available(repo, command):
                continue
            for mode in ["git", "sley_cli", "sley_harness"]:
                print(f"memory {repo['size']} {repo['name']} {mode} {command['name']}")
                rss_runs = []
                errors = []
                for index in range(runs_per_case):
                    bench_repo = case_repo_path(
                        repo_path,
                        repo,
                        command,
                        mode,
                        "memory",
                        scratch_root,
                        git_bin,
                        index,
                    )
                    if mode == "sley_harness":
                        argv = [
                            str(harness_bin),
                            "--repo",
                            str(bench_repo),
                            "--warmup",
                            "0",
                            "--repeat",
                            "1",
                            "--out",
                            str(
                                mem_dir
                                / (
                                    f"memory-{repo['size']}-{repo['name']}-{mode}-"
                                    f"{command['name']}-{index}.json"
                                )
                            ),
                            "--",
                            *command["args"],
                        ]
                    else:
                        argv = process_argv(mode, command, bench_repo, sley_bin, git_bin)
                    proc = run(
                        [time_cmd, "-l", *argv],
                        cwd=bench_repo,
                        env=env,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.PIPE,
                        check=False,
                    )
                    stderr = decode(proc.stderr)
                    if proc.returncode != 0:
                        errors.append(
                            {
                                "index": index,
                                "returncode": proc.returncode,
                                "stderr": stderr[-4000:],
                            }
                        )
                        continue
                    rss = parse_max_rss(stderr)
                    if rss is None:
                        errors.append(
                            {
                                "index": index,
                                "returncode": proc.returncode,
                                "stderr": stderr[-4000:],
                                "message": "could not parse maximum resident set size",
                            }
                        )
                        continue
                    rss_runs.append({"index": index, "max_rss_bytes": rss})
                records.append(make_memory_record(repo, command, mode, rss_runs, errors))
    add_memory_ratios(records)
    return records


def parse_max_rss(stderr: str) -> int | None:
    for line in stderr.splitlines():
        stripped = line.strip()
        if stripped.endswith("maximum resident set size"):
            return int(stripped.split()[0])
        if stripped.startswith("Maximum resident set size"):
            return int(stripped.rsplit(" ", 1)[-1]) * 1024
    return None


def make_memory_record(
    repo: dict[str, Any],
    command: dict[str, Any],
    mode: str,
    rss_runs: list[dict[str, Any]],
    errors: list[dict[str, Any]],
) -> dict[str, Any]:
    record: dict[str, Any] = {
        "repo_size": repo["size"],
        "repo_name": repo["name"],
        "repo_head": repo["head"],
        "command_name": command["name"],
        "command_kind": command_kind(command),
        "command": command_label(command),
        "mode": mode,
        "runs": rss_runs,
    }
    if rss_runs:
        values = [run["max_rss_bytes"] for run in rss_runs]
        record.update(
            {
                "min_rss_bytes": min(values),
                "max_rss_bytes": max(values),
                "mean_rss_bytes": statistics.fmean(values),
                "median_rss_bytes": statistics.median(values),
            }
        )
    if errors:
        record["errors"] = errors
    return record


def add_memory_ratios(records: list[dict[str, Any]]) -> None:
    git_means = {}
    for record in records:
        if record.get("mode") == "git" and "mean_rss_bytes" in record:
            git_means[(record["repo_name"], record["command_name"])] = record[
                "mean_rss_bytes"
            ]
    for record in records:
        key = (record.get("repo_name"), record.get("command_name"))
        git_mean = git_means.get(key)
        if git_mean and "mean_rss_bytes" in record:
            record["rss_ratio_vs_git"] = record["mean_rss_bytes"] / git_mean


def write_csv(path: Path, records: list[dict[str, Any]], fields: list[str]) -> None:
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields, extrasaction="ignore")
        writer.writeheader()
        for record in records:
            writer.writerow(record)


def write_markdown(
    path: Path,
    repos: list[dict[str, Any]],
    timing: list[dict[str, Any]],
    memory: list[dict[str, Any]],
    metadata: dict[str, Any],
) -> None:
    with path.open("w", encoding="utf-8") as handle:
        handle.write("# Sley Human Common Command Bench\n\n")
        handle.write("## Environment\n\n")
        handle.write(f"- Timestamp: `{metadata['timestamp']}`\n")
        handle.write(f"- Git: `{metadata['git_version']}` at `{metadata['git_bin']}`\n")
        handle.write(f"- Sley CLI: `{metadata['sley_bin']}`\n")
        handle.write(f"- Sley harness: `{metadata['sley_harness_bin']}`\n")
        handle.write(f"- Platform: `{metadata['platform']}`\n")

        handle.write("## Repositories\n\n")
        handle.write("| size | repo | head | commits | tracked files |\n")
        handle.write("|---|---|---:|---:|---:|\n")
        for repo in repos:
            handle.write(
                f"| {repo['size']} | {repo['name']} | `{repo['head'][:12]}` | "
                f"{repo['commit_count']} | {repo['tracked_files']} |\n"
            )

        handle.write("\n## Timing Mean ms\n\n")
        handle.write("| repo | kind | command | git | sley_cli | sley_harness |\n")
        handle.write("|---|---|---|---:|---:|---:|\n")
        for row in grouped_rows(timing, "mean_ms"):
            handle.write(
                f"| {row['repo']} | {row['kind']} | `{row['command']}` | "
                f"{fmt(row.get('git'))} | "
                f"{fmt(row.get('sley_cli'))} | {fmt(row.get('sley_harness'))} |\n"
            )

        if memory:
            handle.write("\n## Memory Mean RSS MiB\n\n")
            handle.write("| repo | kind | command | git | sley_cli | sley_harness |\n")
            handle.write("|---|---|---|---:|---:|---:|\n")
            for row in grouped_rows(memory, "mean_rss_bytes", mib=True):
                handle.write(
                    f"| {row['repo']} | {row['kind']} | `{row['command']}` | "
                    f"{fmt(row.get('git'))} | "
                    f"{fmt(row.get('sley_cli'))} | {fmt(row.get('sley_harness'))} |\n"
                )


def grouped_rows(
    records: list[dict[str, Any]],
    value_key: str,
    *,
    mib: bool = False,
) -> list[dict[str, Any]]:
    grouped: dict[tuple[str, str], dict[str, Any]] = {}
    for record in records:
        if value_key not in record:
            continue
        key = (record["repo_name"], record["command_name"])
        row = grouped.setdefault(
            key,
            {
                "repo": f"{record['repo_size']}/{record['repo_name']}",
                "kind": record.get("command_kind", "read"),
                "command": record["command"],
            },
        )
        value = record[value_key]
        row[record["mode"]] = value / (1024 * 1024) if mib else value
    return list(grouped.values())


def fmt(value: Any) -> str:
    if value is None:
        return ""
    return f"{float(value):.2f}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--cache-dir",
        type=Path,
        default=Path(tempfile.gettempdir()) / "sley-human-common-repos",
    )
    parser.add_argument("--out-dir", type=Path)
    parser.add_argument(
        "--git-bin",
        type=Path,
        default=default_git_bin(),
        help="git binary to use for the oracle/baseline; defaults to SLEY_TEST_GIT, then SLEY_BENCH_GIT, then PATH",
    )
    parser.add_argument("--repeat", type=int, default=12)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument("--memory-runs", type=int, default=3)
    parser.add_argument("--skip-memory", action="store_true")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--update-repos", action="store_true")
    parser.add_argument(
        "--sizes",
        default="sm,md,lg",
        help="comma-separated repo sizes to run, from sm,md,lg",
    )
    args = parser.parse_args()

    root = repo_root()
    timestamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
    out_dir = args.out_dir or root / "bench-results" / "human-common" / timestamp
    if not out_dir.is_absolute():
        out_dir = root / out_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    latest_marker = root / "bench-results" / "human-common" / "LATEST"
    latest_marker.parent.mkdir(parents=True, exist_ok=True)

    if args.skip_build:
        sley_bin = root / "target/release/sley"
        harness_bin = root / "target/release/sley-human-harness"
    else:
        sley_bin, harness_bin = build_binaries(root)

    sizes = {item.strip() for item in args.sizes.split(",") if item.strip()}
    unknown_sizes = sorted(sizes - {"sm", "md", "lg"})
    if unknown_sizes:
        raise RuntimeError(f"unknown repo sizes: {', '.join(unknown_sizes)}")

    git_bin = args.git_bin
    git_version = decode(run([str(git_bin), "--version"]).stdout).strip()
    repos = ensure_repos(args.cache_dir, args.update_repos, sizes, git_bin)
    with tempfile.TemporaryDirectory(
        prefix="sley-human-common-write-", dir=tempfile.gettempdir()
    ) as scratch:
        scratch_root = Path(scratch)
        timing = run_timing_matrix(
            repos,
            sley_bin,
            harness_bin,
            git_bin,
            out_dir,
            scratch_root,
            args.warmup,
            args.repeat,
        )
        memory = []
        if not args.skip_memory:
            memory = run_memory_matrix(
                repos,
                sley_bin,
                harness_bin,
                git_bin,
                out_dir,
                scratch_root,
                args.memory_runs,
            )

    payload = {
        "timestamp": timestamp,
        "platform": platform.platform(),
        "python": sys.version,
        "workspace": str(root),
        "git_bin": str(git_bin),
        "git_version": git_version,
        "sley_bin": str(sley_bin),
        "sley_harness_bin": str(harness_bin),
        "repeat": args.repeat,
        "warmup": args.warmup,
        "memory_runs": 0 if args.skip_memory else args.memory_runs,
        "repos": repos,
        "commands": COMMANDS,
        "timing": timing,
        "memory": memory,
    }
    with (out_dir / "human-common-results.json").open("w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2)
        handle.write("\n")

    write_csv(
        out_dir / "timing-summary.csv",
        timing,
        [
            "repo_size",
            "repo_name",
            "command_name",
            "command_kind",
            "command",
            "mode",
            "mean_ms",
            "median_ms",
            "stdev_ms",
            "min_ms",
            "max_ms",
            "speedup_vs_git",
            "skipped",
        ],
    )
    write_csv(
        out_dir / "memory-summary.csv",
        memory,
        [
            "repo_size",
            "repo_name",
            "command_name",
            "command_kind",
            "command",
            "mode",
            "mean_rss_bytes",
            "median_rss_bytes",
            "min_rss_bytes",
            "max_rss_bytes",
            "rss_ratio_vs_git",
        ],
    )
    write_markdown(out_dir / "README.md", repos, timing, memory, payload)
    latest_marker.write_text(f"{out_dir}\n", encoding="utf-8")
    print(out_dir)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
