# Sley Human Common Command Bench

## Environment

- Timestamp: `20260627T230422Z`
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
| sm/walkdir | read | `status --short` | 7.42 | 5.05 | 0.44 |
| sm/walkdir | read | `log --oneline -100` | 8.46 | 5.46 | 0.92 |
| sm/walkdir | read | `branch --list` | 8.04 | 5.40 | 0.12 |
| sm/walkdir | read | `tag --list` | 10.33 | 4.32 | 0.08 |
| sm/walkdir | read | `rev-parse --short HEAD` | 7.06 | 4.36 | 0.13 |
| sm/walkdir | write | `branch -f sley-bench-write HEAD` | 7.10 | 4.92 | 0.39 |
| sm/walkdir | write | `tag -f sley-bench-write HEAD` | 7.53 | 4.67 | 0.37 |
| md/ripgrep | read | `status --short` | 9.06 | 6.44 | 1.65 |
| md/ripgrep | read | `log --oneline -100` | 8.08 | 7.27 | 1.39 |
| md/ripgrep | read | `branch --list` | 9.47 | 8.08 | 0.15 |
| md/ripgrep | read | `tag --list` | 7.42 | 4.59 | 0.13 |
| md/ripgrep | read | `rev-parse --short HEAD` | 6.92 | 4.52 | 0.13 |
| md/ripgrep | write | `branch -f sley-bench-write HEAD` | 7.77 | 5.19 | 0.63 |
| md/ripgrep | write | `tag -f sley-bench-write HEAD` | 8.17 | 5.13 | 0.55 |
| lg/git | read | `status --short` | 22.43 | 27.12 | 18.51 |
| lg/git | read | `log --oneline -100` | 10.17 | 27.63 | 1.81 |
| lg/git | read | `branch --list` | 7.10 | 4.61 | 0.28 |
| lg/git | read | `tag --list` | 7.58 | 5.66 | 0.28 |
| lg/git | read | `rev-parse --short HEAD` | 7.00 | 4.32 | 0.12 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 8.63 | 5.98 | 0.97 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 9.36 | 5.38 | 0.94 |

## Memory Mean RSS MiB

| repo | kind | command | git | sley_cli | sley_harness |
|---|---|---|---:|---:|---:|
| sm/walkdir | read | `status --short` | 8.19 | 4.46 | 2.88 |
| sm/walkdir | read | `log --oneline -100` | 7.89 | 4.84 | 2.77 |
| sm/walkdir | read | `branch --list` | 7.24 | 3.30 | 2.08 |
| sm/walkdir | read | `tag --list` | 7.22 | 3.08 | 1.98 |
| sm/walkdir | read | `rev-parse --short HEAD` | 7.23 | 3.30 | 2.14 |
| sm/walkdir | write | `branch -f sley-bench-write HEAD` | 7.63 | 3.89 | 2.33 |
| sm/walkdir | write | `tag -f sley-bench-write HEAD` | 7.87 | 3.45 | 2.30 |
| md/ripgrep | read | `status --short` | 8.43 | 4.91 | 3.28 |
| md/ripgrep | read | `log --oneline -100` | 8.25 | 5.47 | 3.17 |
| md/ripgrep | read | `branch --list` | 7.27 | 3.35 | 2.11 |
| md/ripgrep | read | `tag --list` | 7.25 | 3.40 | 2.06 |
| md/ripgrep | read | `rev-parse --short HEAD` | 7.27 | 3.29 | 2.14 |
| md/ripgrep | write | `branch -f sley-bench-write HEAD` | 7.68 | 4.12 | 2.56 |
| md/ripgrep | write | `tag -f sley-bench-write HEAD` | 7.91 | 3.69 | 2.49 |
| lg/git | read | `status --short` | 10.02 | 27.27 | 24.70 |
| lg/git | read | `log --oneline -100` | 12.93 | 16.14 | 9.28 |
| lg/git | read | `branch --list` | 7.35 | 3.45 | 2.20 |
| lg/git | read | `tag --list` | 7.34 | 4.23 | 2.20 |
| lg/git | read | `rev-parse --short HEAD` | 7.33 | 3.29 | 2.14 |
| lg/git | write | `branch -f sley-bench-write HEAD` | 7.75 | 4.42 | 2.89 |
| lg/git | write | `tag -f sley-bench-write HEAD` | 7.96 | 4.11 | 2.82 |
