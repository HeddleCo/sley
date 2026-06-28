# Sley Human Common Command Bench

## Environment

- Timestamp: `20260628T001902Z`
- Git: `git version 2.54.0` at `/opt/homebrew/bin/git`
- Sley CLI: `/Users/lukethorne/.codex/worktrees/ccac/sley/target/release/sley`
- Sley harness: `/Users/lukethorne/.codex/worktrees/ccac/sley/target/release/sley-human-harness`
- Platform: `macOS-26.5.1-arm64-arm-64bit-Mach-O`
## Repositories

| size | repo | head | commits | tracked files |
|---|---|---:|---:|---:|
| lg | git | `6c3d7b73556d` | 81316 | 4765 |

## Timing Mean ms

| repo | kind | command | git | sley_cli | sley_harness |
|---|---|---|---:|---:|---:|
| lg/git | read | `status --short` | 20.89 | 23.57 | 16.01 |
| lg/git | read | `log --oneline -100` | 10.00 | 6.06 | 1.72 |
| lg/git | read | `branch --list` | 6.50 | 4.04 | 0.26 |
| lg/git | read | `tag --list` | 6.90 | 4.98 | 0.27 |
| lg/git | read | `rev-parse --short HEAD` | 6.24 | 3.79 | 0.12 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 7.80 | 5.43 | 0.94 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 8.12 | 7.30 | 1.33 |

## Memory Mean RSS MiB

| repo | kind | command | git | sley_cli | sley_harness |
|---|---|---|---:|---:|---:|
| lg/git | read | `status --short` | 10.06 | 22.86 | 20.94 |
| lg/git | read | `log --oneline -100` | 12.91 | 10.96 | 9.22 |
| lg/git | read | `branch --list` | 7.30 | 3.40 | 2.20 |
| lg/git | read | `tag --list` | 7.34 | 4.28 | 2.23 |
| lg/git | read | `rev-parse --short HEAD` | 7.31 | 3.28 | 2.14 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 7.75 | 4.33 | 2.83 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 7.96 | 3.91 | 2.79 |
