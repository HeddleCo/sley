# Sley Human Common Command Bench

## Environment

- Timestamp: `20260628T001936Z`
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
| sm/walkdir | read | `status --short` | 7.16 | 4.31 | 0.42 |
| sm/walkdir | read | `log --oneline -100` | 7.35 | 5.08 | 0.93 |
| sm/walkdir | read | `branch --list` | 6.47 | 3.83 | 0.10 |
| sm/walkdir | read | `tag --list` | 6.39 | 4.41 | 0.23 |
| sm/walkdir | read | `rev-parse --short HEAD` | 8.57 | 5.52 | 0.14 |
| sm/walkdir | write | `branch -f sley-bench-write HEAD` | 7.79 | 4.97 | 0.42 |
| sm/walkdir | write | `tag -f sley-bench-write HEAD` | 7.73 | 4.76 | 0.36 |
| md/ripgrep | read | `status --short` | 9.18 | 6.48 | 1.70 |
| md/ripgrep | read | `log --oneline -100` | 8.07 | 5.25 | 1.04 |
| md/ripgrep | read | `branch --list` | 6.92 | 6.09 | 0.16 |
| md/ripgrep | read | `tag --list` | 12.15 | 4.80 | 0.13 |
| md/ripgrep | read | `rev-parse --short HEAD` | 7.03 | 4.47 | 0.12 |
| md/ripgrep | write | `branch -f sley-bench-write HEAD` | 7.67 | 5.11 | 0.56 |
| md/ripgrep | write | `tag -f sley-bench-write HEAD` | 7.87 | 4.86 | 0.48 |
| lg/git | read | `status --short` | 21.77 | 29.59 | 15.93 |
| lg/git | read | `log --oneline -100` | 10.10 | 6.49 | 1.67 |
| lg/git | read | `branch --list` | 7.35 | 4.55 | 0.27 |
| lg/git | read | `tag --list` | 7.60 | 5.47 | 0.28 |
| lg/git | read | `rev-parse --short HEAD` | 6.90 | 4.21 | 0.12 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 8.46 | 5.51 | 0.99 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 8.58 | 7.79 | 0.98 |

## Memory Mean RSS MiB

| repo | kind | command | git | sley_cli | sley_harness |
|---|---|---|---:|---:|---:|
| sm/walkdir | read | `status --short` | 8.19 | 4.38 | 2.77 |
| sm/walkdir | read | `log --oneline -100` | 7.90 | 4.42 | 2.72 |
| sm/walkdir | read | `branch --list` | 7.23 | 3.27 | 2.08 |
| sm/walkdir | read | `tag --list` | 7.23 | 3.09 | 2.02 |
| sm/walkdir | read | `rev-parse --short HEAD` | 7.25 | 3.29 | 2.14 |
| sm/walkdir | write | `branch -f sley-bench-write HEAD` | 7.65 | 3.80 | 2.30 |
| sm/walkdir | write | `tag -f sley-bench-write HEAD` | 7.86 | 3.41 | 2.26 |
| md/ripgrep | read | `status --short` | 8.47 | 4.65 | 3.12 |
| md/ripgrep | read | `log --oneline -100` | 8.23 | 4.83 | 3.09 |
| md/ripgrep | read | `branch --list` | 7.29 | 3.31 | 2.11 |
| md/ripgrep | read | `tag --list` | 7.26 | 3.45 | 2.09 |
| md/ripgrep | read | `rev-parse --short HEAD` | 7.27 | 3.27 | 2.14 |
| md/ripgrep | write | `branch -f sley-bench-write HEAD` | 7.74 | 4.05 | 2.51 |
| md/ripgrep | write | `tag -f sley-bench-write HEAD` | 7.93 | 3.62 | 2.50 |
| lg/git | read | `status --short` | 10.03 | 22.58 | 20.85 |
| lg/git | read | `log --oneline -100` | 12.91 | 10.91 | 9.23 |
| lg/git | read | `branch --list` | 7.31 | 3.41 | 2.20 |
| lg/git | read | `tag --list` | 7.34 | 4.30 | 2.23 |
| lg/git | read | `rev-parse --short HEAD` | 7.31 | 3.28 | 2.14 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 7.72 | 4.33 | 2.84 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 7.97 | 3.91 | 2.79 |
