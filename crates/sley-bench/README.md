# sley-bench — sley vs git comparison suite

Criterion benchmarks that run the same operation through BOTH the release
`sley` binary and a real upstream `git` binary, so every report reads
side-by-side (`<group>/sley` vs `<group>/git`).

## Quiet-box run (the authoritative numbers)

Numbers taken on a loaded box are noise. Before a measured run:

1. **Check the load:** `uptime` — 1-minute load average must be ≤ ~2.5.
   If it is higher, wait; do not run benches concurrently with builds or
   other agents.
2. **Run serially in an isolated target dir** (never the shared workspace
   target — a `cargo bench` on a shared target competes for the cargo build
   lock with sibling builds):

```bash
export PATH="/tmp/git-2.54/bin:$PATH"        # pin the comparison git build
export CARGO_TARGET_DIR=/tmp/sley-bench-target
export GIT_BENCH_BIN=/tmp/git-2.54/bin/git   # explicit pin for the git arm

git --version                                 # expect: git version 2.54.0

# Build the sley binary the benches exec (build.rs deliberately does NOT
# build it — a nested cargo build deadlocks on the workspace lock):
cargo build --release -p sley-cli --bin sley

# Full suite, serial (criterion runs benches sequentially within one cargo
# invocation; do not run multiple cargo bench processes at once):
cargo bench -p sley-bench
```

A full run takes on the order of 10 minutes on a quiet box. To run a single
bench target: `cargo bench -p sley-bench --bench cat_file`. For a quick
smoke pass (validity, not numbers):

```bash
cargo bench -p sley-bench --bench <name> -- --sample-size 10 --warm-up-time 0.3 --measurement-time 1 --noplot
```

## Environment knobs

| Variable | Default | Meaning |
|---|---|---|
| `SLEY_BENCH_BIN` | computed by `build.rs` (`$CARGO_TARGET_DIR/release/sley`) | sley binary the benches exec |
| `GIT_BENCH_BIN` | `git` (from `PATH`) | git binary for the comparison arm |

## What is covered

| Bench target | Group(s) | Commands | Arms |
|---|---|---|---|
| `cat_file` | `cat_file_p_single_packed`, `cat_file_batch_check`, `cat_file_batch` | `cat-file -p`, `--batch-check`, `--batch` (100/500 oids per process) | sley vs git (+ sley-internal ODB arm) |
| `rev_parse` | `rev_parse_oid_resolve` | `rev-parse` with 1/100/500 oids per invocation | sley vs git (+ internal `resolve_prefix`) |
| `count_objects` | `count_objects_verbose_packed` | `count-objects -v` | sley vs git |
| `rev_list` | `rev_list_count_head`, `rev_list_oneline_head` | `rev-list --count HEAD`, `rev-list --oneline -100` | sley vs git |
| `ref_walk` | `for_each_ref`, `for_each_ref_format`, `show_ref` | ref enumeration over 100 branch refs | sley vs git |
| `tree_walk` | `ls_tree_recursive_head` | `ls-tree -r HEAD` | sley vs git |
| `log_pretty` | `log_oneline_200`, `log_format_200`, `log_oneline_full_history` | `log` pretty formats over 1k commits | sley vs git |
| `config_cmd` | `config_get`, `config_list`, `config_get_regexp` | `config` reads over 50 `bench.*` keys | sley vs git |
| `index_ops` | `ls_files`, `ls_files_stage`, `update_index_refresh`, `hash_object_stdin_paths` | index plumbing on a 1k-file worktree | sley vs git |
| `worktree_ops` | `status_porcelain`, `add_update_10_dirty`, `commit_allow_empty` | porcelain on a 1k-file worktree | sley vs git |
| `init_cmd` | `init_fresh_dir` | `init -q -b main` into a fresh dir (incl. mkdir/rm, symmetric) | sley vs git |
| `pack_install` | `install_pack` | internal `FileObjectDatabase::install_pack` on a 500-blob deltified pack | sley-internal only |
| `batch_check_profile` | `batch_check_components` | component breakdown of the `--batch-check` hot path | sley-internal only |

## Fixtures

Deterministic synthetic repos generated once per bench process (see
`src/lib.rs` and `examples/setup_fixtures.rs`):

- **Pack fixture** — 500 deltifiable blobs in a single pack, no loose objects.
- **Commit fixture** — 1,000-commit linear history + commit-graph + 100
  branch refs, built in-process via sley internals (verified readable by
  upstream git).
- **Worktree fixture** — 1,000 tracked files across 50 dirs, 5 commits,
  identity + 50 `bench.*` config keys; built with the comparison `git`
  binary (the oracle) so a sley write-path bug cannot poison it. Mutating
  benches build one fixture per arm so the binaries never share state.

Sizes are deliberately moderate so the full suite finishes in ~10 minutes;
process-spawn overhead dominates micro-ops, so benches prefer batch forms
(one process over many objects/refs/commits) to measure work, not fork+exec.

There is also a hyperfine-based wall-clock harness at
`scripts/bench-vs-git.sh` (same fixtures, exported via
`cargo run -p sley-bench --example setup_fixtures`).
