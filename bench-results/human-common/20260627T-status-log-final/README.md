# Sley Human Common Command Bench

## Environment

- Timestamp: `20260628T002727Z`
- Git: `git version 2.54.0` at `/opt/homebrew/bin/git`
- Sley CLI: `/Users/lukethorne/.codex/worktrees/ccac/sley/target/release/sley`
- Sley harness: `/Users/lukethorne/.codex/worktrees/ccac/sley/target/release/sley-human-harness`
- Platform: `macOS-26.5.1-arm64-arm-64bit-Mach-O`
## Repositories

| size | repo | head | commits | tracked files |
|---|---|---:|---:|---:|
| sm | walkdir | `6fd031c82ba5` | 192 | 20 |
| md | ripgrep | `dfe4a81d2591` | 2217 | 222 |
| lg | git | `6c3d7b73556d` | 81316 | 4765 |

## Timing Mean ms

| repo | kind | command | git | sley_cli | sley_harness |
|---|---|---|---:|---:|---:|
| sm/walkdir | read | `status --short` | 12.08 | 8.57 | 0.74 |
| sm/walkdir | read | `log --oneline -100` | 12.09 | 7.56 | 1.49 |
| sm/walkdir | read | `branch --list` | 10.33 | 6.31 | 0.12 |
| sm/walkdir | read | `tag --list` | 10.13 | 6.75 | 0.10 |
| sm/walkdir | read | `rev-parse --short HEAD` | 9.89 | 6.37 | 0.15 |
| sm/walkdir | write | `branch -f sley-bench-write HEAD` | 11.16 | 7.31 | 2.04 |
| sm/walkdir | write | `tag -f sley-bench-write HEAD` | 17.21 | 7.17 | 0.63 |
| md/ripgrep | read | `status --short` | 12.95 | 9.65 | 2.49 |
| md/ripgrep | read | `log --oneline -100` | 13.31 | 8.31 | 1.38 |
| md/ripgrep | read | `branch --list` | 10.74 | 12.42 | 1.06 |
| md/ripgrep | read | `tag --list` | 20.19 | 7.00 | 0.38 |
| md/ripgrep | read | `rev-parse --short HEAD` | 10.52 | 6.46 | 0.18 |
| md/ripgrep | write | `branch -f sley-bench-write HEAD` | 10.12 | 6.76 | 0.73 |
| md/ripgrep | write | `tag -f sley-bench-write HEAD` | 10.91 | 6.92 | 1.08 |
| lg/git | read | `status --short` | 33.78 | 25.17 | 15.76 |
| lg/git | read | `log --oneline -100` | 12.40 | 8.58 | 3.64 |
| lg/git | read | `branch --list` | 17.51 | 8.47 | 0.53 |
| lg/git | read | `tag --list` | 11.72 | 8.14 | 0.51 |
| lg/git | read | `rev-parse --short HEAD` | 10.04 | 6.08 | 0.15 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 10.94 | 7.32 | 1.89 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 16.86 | 7.43 | 1.19 |

## Memory Mean RSS MiB

| repo | kind | command | git | sley_cli | sley_harness |
|---|---|---|---:|---:|---:|
| sm/walkdir | read | `status --short` | 8.20 | 4.40 | 2.77 |
| sm/walkdir | read | `log --oneline -100` | 7.89 | 4.45 | 2.76 |
| sm/walkdir | read | `branch --list` | 7.23 | 3.30 | 2.07 |
| sm/walkdir | read | `tag --list` | 7.23 | 3.10 | 2.02 |
| sm/walkdir | read | `rev-parse --short HEAD` | 7.25 | 3.28 | 2.12 |
| sm/walkdir | write | `branch -f sley-bench-write HEAD` | 7.65 | 3.86 | 2.31 |
| sm/walkdir | write | `tag -f sley-bench-write HEAD` | 7.91 | 3.41 | 2.27 |
| md/ripgrep | read | `status --short` | 8.45 | 4.70 | 3.07 |
| md/ripgrep | read | `log --oneline -100` | 8.22 | 4.84 | 3.10 |
| md/ripgrep | read | `branch --list` | 7.28 | 3.35 | 2.10 |
| md/ripgrep | read | `tag --list` | 7.28 | 3.45 | 2.09 |
| md/ripgrep | read | `rev-parse --short HEAD` | 7.28 | 3.30 | 2.14 |
| md/ripgrep | write | `branch -f sley-bench-write HEAD` | 7.73 | 4.05 | 2.58 |
| md/ripgrep | write | `tag -f sley-bench-write HEAD` | 7.97 | 3.64 | 2.47 |
| lg/git | read | `status --short` | 10.02 | 8.39 | 6.20 |
| lg/git | read | `log --oneline -100` | 12.91 | 10.91 | 9.23 |
| lg/git | read | `branch --list` | 7.30 | 3.44 | 2.19 |
| lg/git | read | `tag --list` | 7.32 | 4.27 | 2.21 |
| lg/git | read | `rev-parse --short HEAD` | 7.31 | 3.28 | 2.14 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 7.78 | 4.35 | 2.98 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 7.98 | 3.93 | 2.81 |
