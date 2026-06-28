# Sley Human Common Command Bench

## Environment

- Timestamp: `20260628T002700Z`
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
| lg/git | read | `status --short` | 20.16 | 19.55 | 12.75 |
| lg/git | read | `log --oneline -100` | 9.29 | 6.28 | 1.69 |
| lg/git | read | `branch --list` | 7.20 | 4.40 | 0.27 |
| lg/git | read | `tag --list` | 7.52 | 5.46 | 0.28 |
| lg/git | read | `rev-parse --short HEAD` | 7.12 | 4.55 | 0.13 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 7.69 | 5.56 | 1.03 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 7.77 | 7.36 | 1.06 |

## Memory Mean RSS MiB

| repo | kind | command | git | sley_cli | sley_harness |
|---|---|---|---:|---:|---:|
| lg/git | read | `status --short` | 10.09 | 8.43 | 6.20 |
| lg/git | read | `log --oneline -100` | 12.95 | 10.91 | 9.23 |
| lg/git | read | `branch --list` | 7.28 | 3.42 | 2.19 |
| lg/git | read | `tag --list` | 7.34 | 4.27 | 2.22 |
| lg/git | read | `rev-parse --short HEAD` | 7.31 | 3.30 | 2.13 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 7.73 | 4.44 | 2.84 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 7.98 | 3.92 | 2.80 |
