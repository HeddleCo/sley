# Sley Human Common Command Bench

## Environment

- Timestamp: `20260628T001754Z`
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
| lg/git | read | `status --short` | 21.42 | 23.86 |  |
| lg/git | read | `log --oneline -100` | 9.96 | 5.97 |  |
| lg/git | read | `branch --list` | 6.98 | 4.28 |  |
| lg/git | read | `tag --list` | 7.14 | 5.15 |  |
| lg/git | read | `rev-parse --short HEAD` | 6.65 | 3.90 |  |
| lg/git | write | `branch -f sley-bench-write HEAD` | 8.33 | 5.12 |  |
| lg/git | write | `tag -f sley-bench-write HEAD` | 8.26 | 5.05 |  |

## Memory Mean RSS MiB

| repo | kind | command | git | sley_cli | sley_harness |
|---|---|---|---:|---:|---:|
| lg/git | read | `status --short` | 10.06 | 22.72 |  |
| lg/git | read | `log --oneline -100` | 12.93 | 10.96 |  |
| lg/git | read | `branch --list` | 7.31 | 3.40 |  |
| lg/git | read | `tag --list` | 7.33 | 4.28 |  |
| lg/git | read | `rev-parse --short HEAD` | 7.31 | 3.27 |  |
| lg/git | write | `branch -f sley-bench-write HEAD` | 7.74 | 4.35 |  |
| lg/git | write | `tag -f sley-bench-write HEAD` | 7.95 | 3.92 |  |
