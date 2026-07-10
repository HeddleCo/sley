# sley

An embeddable Rust Git-equivalent library with a thin, byte-compatible CLI
wrapper.

The pinned compatibility oracle is upstream Git 2.55.0. Repository semantics
live in typed engine crates and the `sley::Repository` facade; the CLI owns
argv/environment setup, terminal integration, dispatch, and Git-identical byte
rendering. The curated conformance harness runs the oracle and Sley separately
and compares exact TAP cells.

This repository is not a complete Git replacement yet. Tracked gaps are explicit
in [`PARITY.md`](PARITY.md).

## Current Commands

```sh
cargo test --workspace
cargo run -p sley-cli -- init [-q] [--bare] [-b <branch>|--initial-branch=<branch>] /tmp/repo
cargo run -p sley-cli -- add <path>
cargo run -p sley-cli -- branch [<name> [<start>]]
cargo run -p sley-cli -- branch --list
cargo run -p sley-cli -- branch --list <pattern>...
cargo run -p sley-cli -- branch -r
cargo run -p sley-cli -- branch -r --list <pattern>...
cargo run -p sley-cli -- branch -a
cargo run -p sley-cli -- branch -a --list <pattern>...
cargo run -p sley-cli -- branch --points-at <object-ish>
cargo run -p sley-cli -- branch --contains <commit-ish>
cargo run -p sley-cli -- branch --no-contains <commit-ish>
cargo run -p sley-cli -- branch --merged [<commit-ish>]
cargo run -p sley-cli -- branch --no-merged [<commit-ish>]
cargo run -p sley-cli -- branch --show-current
cargo run -p sley-cli -- branch -d <name>...
cargo run -p sley-cli -- branch -D <name>...
cargo run -p sley-cli -- branch --force <name> [<start>]
cargo run -p sley-cli -- checkout <branch>
cargo run -p sley-cli -- diff --name-status
cargo run -p sley-cli -- diff --name-only
cargo run -p sley-cli -- diff --exit-code --name-only
cargo run -p sley-cli -- diff --quiet
cargo run -p sley-cli -- diff --name-status HEAD
cargo run -p sley-cli -- diff --name-only HEAD
cargo run -p sley-cli -- diff --exit-code --name-only HEAD
cargo run -p sley-cli -- diff --quiet HEAD
cargo run -p sley-cli -- diff --cached --name-status
cargo run -p sley-cli -- diff --cached --name-only
cargo run -p sley-cli -- diff --cached --exit-code --name-only
cargo run -p sley-cli -- diff --cached --quiet
cargo run -p sley-cli -- diff --cached --name-status HEAD
cargo run -p sley-cli -- diff --cached --name-only HEAD
cargo run -p sley-cli -- diff --cached --exit-code --name-only HEAD
cargo run -p sley-cli -- diff --cached --quiet HEAD
cargo run -p sley-cli -- diff --staged --name-status HEAD
cargo run -p sley-cli -- diff --staged --name-only HEAD
cargo run -p sley-cli -- for-each-ref [--count=<n>] [--sort=<key>] [--ignore-case] [--start-after=<marker>] [--exclude=<pattern>] [--points-at=<object>] [--contains[=<commit>]|--no-contains[=<commit>]] [--merged[=<commit>]|--no-merged[=<commit>]] [--include-root-refs] [--omit-empty] [--format=<format>] [<prefix>...]
cargo run -p sley-cli -- hash-object --stdin
cargo run -p sley-cli -- hash-object <path>...
cargo run -p sley-cli -- hash-object --stdin <path>...
cargo run -p sley-cli -- hash-object [--filters|--no-filters|--literally|--no-literally] [--path=<path>] --stdin
cargo run -p sley-cli -- hash-object --stdin-paths [--no-filters]
cargo run -p sley-cli -- hash-object --object-format=sha256 --stdin
cargo run -p sley-cli -- hash-object -w --stdin
cargo run -p sley-cli -- cat-file -e <object-or-rev>
cargo run -p sley-cli -- cat-file -t <object-or-rev>
cargo run -p sley-cli -- cat-file -s <object-or-rev>
cargo run -p sley-cli -- cat-file -p <object-or-rev>
cargo run -p sley-cli -- cat-file --batch
cargo run -p sley-cli -- cat-file --batch=<format>
cargo run -p sley-cli -- cat-file --batch-check
cargo run -p sley-cli -- cat-file --batch-check=<format>
cargo run -p sley-cli -- commit -m <message>
cargo run -p sley-cli -- commit-tree <tree-id> -m <message>
cargo run -p sley-cli -- ls-files [--stage|-s] [--cached|-c] [--others|-o] [--directory] [--no-empty-directory] [--deleted|-d] [--modified|-m] [--unmerged|-u] [--deduplicate] [--error-unmatch] [--full-name] [-z] [<path>...]
cargo run -p sley-cli -- ls-files [--stage|-s] -- <path>...
cargo run -p sley-cli -- ls-tree [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --name-only [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --name-status [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --object-only [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --long [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree -t [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree -d [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree -r [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree -r -t [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree -r -d [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --abbrev[=<n>] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --no-abbrev <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --full-name <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --no-full-name <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --full-tree <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --no-full-tree <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree --format <format> <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree <tree-ish> -- <path>...
cargo run -p sley-cli -- ls-tree -r --name-only [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree -r --object-only [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- ls-tree -r --long [-z] <tree-ish> [<path>...]
cargo run -p sley-cli -- log [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p sley-cli -- log --oneline [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p sley-cli -- log --reverse --oneline [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p sley-cli -- log --pretty=oneline [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p sley-cli -- log --format=oneline [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p sley-cli -- log --format=<format> [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p sley-cli -- log --pretty=format:<format> [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p sley-cli -- rev-parse <rev>...
cargo run -p sley-cli -- rev-parse --abbrev-ref <rev>...
cargo run -p sley-cli -- rev-parse --symbolic-full-name <rev>...
cargo run -p sley-cli -- rev-parse --verify [--quiet] [--end-of-options] <rev> [--]
cargo run -p sley-cli -- rev-parse --short[=<n>] <rev>
cargo run -p sley-cli -- rev-parse --path-format=<absolute|relative> <path-option>...
cargo run -p sley-cli -- rev-parse --git-dir
cargo run -p sley-cli -- rev-parse --absolute-git-dir
cargo run -p sley-cli -- rev-parse --git-common-dir
cargo run -p sley-cli -- rev-parse --git-path <path>
cargo run -p sley-cli -- rev-parse --resolve-git-dir <path>
cargo run -p sley-cli -- rev-parse --show-toplevel
cargo run -p sley-cli -- rev-parse --show-prefix
cargo run -p sley-cli -- rev-parse --show-cdup
cargo run -p sley-cli -- rev-parse --show-superproject-working-tree
cargo run -p sley-cli -- rev-parse --show-object-format
cargo run -p sley-cli -- rev-parse --show-ref-format
cargo run -p sley-cli -- rev-parse --local-env-vars
cargo run -p sley-cli -- rev-parse --is-inside-work-tree
cargo run -p sley-cli -- rev-parse --is-inside-git-dir
cargo run -p sley-cli -- rev-parse --is-bare-repository
cargo run -p sley-cli -- rev-parse --is-shallow-repository
cargo run -p sley-cli -- write-tree
cargo run -p sley-cli -- write-tree --prefix=<prefix>
cargo run -p sley-cli -- write-tree --missing-ok
cargo run -p sley-cli -- update-index [--add] [--remove|--force-remove] [--chmod=(+|-)x] [--cacheinfo <mode>,<object>,<path>] [--stdin|--index-info] [-z] <path>...
cargo run -p sley-cli -- update-ref [--deref|--no-deref] refs/heads/main <object-id> [<old-object-id>]
cargo run -p sley-cli -- update-ref -d <ref>
cargo run -p sley-cli -- show-ref
cargo run -p sley-cli -- show-ref --head
cargo run -p sley-cli -- show-ref --heads
cargo run -p sley-cli -- show-ref --branches
cargo run -p sley-cli -- show-ref --tags
cargo run -p sley-cli -- show-ref --branches --tags
cargo run -p sley-cli -- show-ref --dereference [--tags]
cargo run -p sley-cli -- show-ref --dereference --no-dereference [--tags]
cargo run -p sley-cli -- show-ref --hash|--no-hash [--branches|--heads|--tags]
cargo run -p sley-cli -- show-ref --abbrev[=<n>] [--no-abbrev] [--branches|--heads|--tags]
cargo run -p sley-cli -- show-ref <pattern>...
cargo run -p sley-cli -- show-ref -- <ref>...
cargo run -p sley-cli -- show-ref --verify <ref>
cargo run -p sley-cli -- show-ref --verify -- <ref>...
cargo run -p sley-cli -- show-ref --verify --dereference <ref>
cargo run -p sley-cli -- show-ref --verify --hash <ref>
cargo run -p sley-cli -- show-ref --verify --quiet <ref>
cargo run -p sley-cli -- show-ref --exists <ref>
cargo run -p sley-cli -- show-ref --exclude-existing[=<pattern>]
cargo run -p sley-cli -- symbolic-ref [--short] [--no-recurse] <name>
cargo run -p sley-cli -- symbolic-ref [--quiet] <name>
cargo run -p sley-cli -- symbolic-ref --delete <name>
cargo run -p sley-cli -- symbolic-ref [-m <reason>] <name> <ref>
cargo run -p sley-cli -- status --short
cargo run -p sley-cli -- status --short [-z|--null]
cargo run -p sley-cli -- status --short --branch
cargo run -p sley-cli -- status --short --branch --no-branch
cargo run -p sley-cli -- status --short [-u|-uall|-unormal|-uno|--untracked-files[=all|normal|no]]
cargo run -p sley-cli -- status --porcelain
cargo run -p sley-cli -- status --porcelain=1
cargo run -p sley-cli -- status --porcelain [-z|--null]
cargo run -p sley-cli -- status --porcelain --branch
cargo run -p sley-cli -- status --porcelain [-u|-uall|-unormal|-uno|--untracked-files[=all|normal|no]]
cargo run -p sley-cli -- status --porcelain=v1
cargo run -p sley-cli -- status --porcelain=v1 [-z|--null]
cargo run -p sley-cli -- status --porcelain=v1 --branch
cargo run -p sley-cli -- status --porcelain=v1 [-u|-uall|-unormal|-uno|--untracked-files[=all|normal|no]]
cargo run -p sley-cli -- stash list [--oneline|--format=<format>|--pretty=format:<format>] [--grep=<pattern>] [--all-match|--invert-grep|-i|--regexp-ignore-case|-F|--fixed-strings] [--abbrev[=<n>]|--no-abbrev] [-<n>|-n <n>|--max-count=<n>]
cargo run -p sley-cli -- stash clear
cargo run -p sley-cli -- stash drop [-q|--quiet|--no-quiet] [stash@{<n>}]
cargo run -p sley-cli -- stash show [-u|--include-untracked|--only-untracked] [--diff-filter=<filter>] [--raw|--stat|--compact-summary|--numstat|--shortstat|--summary|--name-only|--name-status|-p|--patch|--patch-with-raw|--patch-with-stat|--oneline|--quiet] [stash@{<n>}]
cargo run -p sley-cli -- stash store [-m <message>|--message=<message>] [-q|--quiet|--no-quiet] <commit>
cargo run -p sley-cli -- tag [<name> [<target>]]
cargo run -p sley-cli -- tag --list
cargo run -p sley-cli -- tag --list <pattern>...
cargo run -p sley-cli -- tag -l <pattern>...
cargo run -p sley-cli -- tag --points-at <object-ish> [<pattern>...]
cargo run -p sley-cli -- tag --contains <commit-ish> [<pattern>...]
cargo run -p sley-cli -- tag --no-contains <commit-ish> [<pattern>...]
cargo run -p sley-cli -- tag --merged [<commit-ish>]
cargo run -p sley-cli -- tag --no-merged [<commit-ish>]
cargo run -p sley-cli -- tag -f <name> [<target>]
cargo run -p sley-cli -- tag -a <name> -m <message> [<target>]
cargo run -p sley-cli -- tag -f -a <name> -m <message> [<target>]
cargo run -p sley-cli -- tag -d <name>...
cargo run -p sley-cli -- testkit hash-object
cargo run -p sley-cli -- testkit hash-object-sha256
cargo run -p sley-cli -- testkit loose-sha256
cargo run -p sley-cli -- testkit config
cargo run -p sley-cli -- testkit commit
cargo run -p sley-cli -- testkit commit-tree
cargo run -p sley-cli -- testkit branch
cargo run -p sley-cli -- testkit branch-current
cargo run -p sley-cli -- testkit branch-delete
cargo run -p sley-cli -- testkit checkout
cargo run -p sley-cli -- testkit tag
cargo run -p sley-cli -- testkit tag-delete
cargo run -p sley-cli -- testkit annotated-tag
cargo run -p sley-cli -- testkit diff
cargo run -p sley-cli -- testkit rev-parse
cargo run -p sley-cli -- testkit rev-parse-parents
cargo run -p sley-cli -- testkit rev-parse-peel
cargo run -p sley-cli -- testkit rev-parse-object-format
cargo run -p sley-cli -- testkit add-status
cargo run -p sley-cli -- testkit index
cargo run -p sley-cli -- testkit update-index
cargo run -p sley-cli -- testkit ls-files
cargo run -p sley-cli -- testkit update-ref-delete
cargo run -p sley-cli -- testkit write-tree
cargo run -p sley-cli -- testkit ls-tree
cargo run -p sley-cli -- testkit cat-file
cargo run -p sley-cli -- testkit log
cargo run -p sley-cli -- testkit pack-read
cargo run -p sley-cli -- testkit packed-odb
cargo run -p sley-cli -- testkit pack-index
cargo run -p sley-cli -- testkit pack-write
cargo run -p sley-cli -- testkit refs
cargo run -p sley-cli -- testkit show-ref
cargo run -p sley-cli -- testkit show-ref-verify
cargo run -p sley-cli -- testkit symbolic-ref
```

## Upstream suite timing

The original 2026-07-10 Git v2.55.0 baseline ran 891 enrolled scripts on an
M1 Pro, but its Sley side used a generated `/bin/sh` shim for every command
while Git ran directly. That candidate-only process launch makes the historical
aggregate and tail timings unsuitable for release performance claims. Its
equal-work classification remains useful: only 425 scripts did demonstrably
equal work, and failing/short-circuiting rows remain excluded.

The harness now launches Sley directly under the installed `git` name. In the
first corrected three-pair shard, exact `t7004-tag.sh` (231/231 cells) measured
5.744 s for Git and 5.297 s for Sley: a 1.084× paired-median speedup, with the
selected-run aggregate, median, p95/p99, and wall-time comparisons all passing.
After reference-fsync policy and backend-selection caching, a five-script refs
shard retained exact work in three alternating trials and measured a 0.796
Sley/Git aggregate ratio, 1.265× median speedup, and 0.79 p95/p99 ratios; every
script beat Git, including `t1460-refs-migrate.sh` at 1.144×. After
repack/bitmap optimization,
exact `t5333-pseudo-merge-bitmaps.sh` improved from 10.783 s to 8.081 s on
Sley, but Git's 6.633 s median still leaves Sley at 0.821× and that shard's
performance gates remain red. Exact `t7063-status-untracked-cache.sh` is now
1.987× faster on Sley; exact `t3311-notes-merge-fanout.sh` is 1.045× faster in
the latest sample but remains close enough to the 1.05× threshold to require
the dedicated Linux certification run.

The first clean-oracle, direct-launch full SHA-1 matrix completed all 891
scripts without an abort or timeout. Git v2.55.0 established 881 passing
scripts plus 10 legitimate skip-all scripts; Sley reported 496 passing, 385
failing, and the same 10 skip-all scripts. Exact per-cell comparison is the
release measure: 513/891 scripts match every oracle-applicable cell, while 378
remain incomparable. A one-shot equal-work diagnostic over the 496 scripts
that also passed end-to-end has Sley ahead in aggregate, median, p95, and p99,
but it is not an alternating paired run and is not a certification result.

The latest integrated eight-wave rerun raised the exact result to 556/891 and
the raw result to 538 passing, 343 failing, and 10 skip-all scripts
(30,103/32,357 assertions, 93%). A subsequently verified MIDX regression
repair (`t5319`, 98/98) and credential-engine closure (`t0300`, 56/56) establish
a current verified exact floor of 557/891. `t0301-credential-cache.sh` was exact
before its daemon-latency change, but the managed sandbox now rejects its Unix
socket bind, so it is not carried into that floor until an unsandboxed rerun.
On the 538 exact end-to-end rows in the integrated run,
the non-alternating equal-work diagnostic measured a 0.749 Sley/Git aggregate
ratio, 1.227× median paired speedup, and 0.723/0.696 p95/p99 ratios. These are
development diagnostics only: at least 334 correctness gaps remain and the five-pair
Linux certification has not run. After caching repeated parent-directory
metadata probes in status, a fresh 21-case, byte-identical common-command sample
passes both gates: 0.811 Sley/Git geometric mean and no row over 1.05. The
former blocker, large-repository `status --short`, now measures 22.87 ms for
Sley versus 23.17 ms for Git. Eight-wave wall time is still a separate,
not-yet-certified measure.

This is targeted development evidence, not the required five-pair dedicated
Linux certification. [`crates/sley-testkit/UPSTREAM_TIMINGS.md`](crates/sley-testkit/UPSTREAM_TIMINGS.md)
records both the superseded baseline and the corrected measurement protocol.
