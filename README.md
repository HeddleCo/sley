# git-rs

Greenfield Rust workspace for a minimal-dependency Git-compatible implementation.

The compatibility target is upstream Git 2.54.0. The first deliverable is the crate
topology, typed storage-core APIs, and a conformance harness that can compare
selected behavior with the system `git` binary.

This repository is not a complete Git replacement yet. Tracked gaps are explicit
in [`PARITY.md`](PARITY.md).

## Current Commands

```sh
cargo test --workspace
cargo run -p git-cli -- init [-q] [--bare] [-b <branch>|--initial-branch=<branch>] /tmp/repo
cargo run -p git-cli -- add <path>
cargo run -p git-cli -- branch [<name> [<start>]]
cargo run -p git-cli -- branch --list
cargo run -p git-cli -- branch --list <pattern>...
cargo run -p git-cli -- branch -r
cargo run -p git-cli -- branch -r --list <pattern>...
cargo run -p git-cli -- branch -a
cargo run -p git-cli -- branch -a --list <pattern>...
cargo run -p git-cli -- branch --points-at <object-ish>
cargo run -p git-cli -- branch --contains <commit-ish>
cargo run -p git-cli -- branch --no-contains <commit-ish>
cargo run -p git-cli -- branch --merged [<commit-ish>]
cargo run -p git-cli -- branch --no-merged [<commit-ish>]
cargo run -p git-cli -- branch --show-current
cargo run -p git-cli -- branch -d <name>...
cargo run -p git-cli -- branch -D <name>...
cargo run -p git-cli -- branch --force <name> [<start>]
cargo run -p git-cli -- checkout <branch>
cargo run -p git-cli -- diff --name-status
cargo run -p git-cli -- diff --name-only
cargo run -p git-cli -- diff --exit-code --name-only
cargo run -p git-cli -- diff --quiet
cargo run -p git-cli -- diff --name-status HEAD
cargo run -p git-cli -- diff --name-only HEAD
cargo run -p git-cli -- diff --exit-code --name-only HEAD
cargo run -p git-cli -- diff --quiet HEAD
cargo run -p git-cli -- diff --cached --name-status
cargo run -p git-cli -- diff --cached --name-only
cargo run -p git-cli -- diff --cached --exit-code --name-only
cargo run -p git-cli -- diff --cached --quiet
cargo run -p git-cli -- diff --cached --name-status HEAD
cargo run -p git-cli -- diff --cached --name-only HEAD
cargo run -p git-cli -- diff --cached --exit-code --name-only HEAD
cargo run -p git-cli -- diff --cached --quiet HEAD
cargo run -p git-cli -- diff --staged --name-status HEAD
cargo run -p git-cli -- diff --staged --name-only HEAD
cargo run -p git-cli -- for-each-ref [--count=<n>] [--sort=<key>] [--ignore-case] [--start-after=<marker>] [--exclude=<pattern>] [--points-at=<object>] [--contains[=<commit>]|--no-contains[=<commit>]] [--merged[=<commit>]|--no-merged[=<commit>]] [--include-root-refs] [--omit-empty] [--format=<format>] [<prefix>...]
cargo run -p git-cli -- hash-object --stdin
cargo run -p git-cli -- hash-object <path>...
cargo run -p git-cli -- hash-object --stdin <path>...
cargo run -p git-cli -- hash-object [--filters|--no-filters|--literally|--no-literally] [--path=<path>] --stdin
cargo run -p git-cli -- hash-object --stdin-paths [--no-filters]
cargo run -p git-cli -- hash-object --object-format=sha256 --stdin
cargo run -p git-cli -- hash-object -w --stdin
cargo run -p git-cli -- cat-file -e <object-or-rev>
cargo run -p git-cli -- cat-file -t <object-or-rev>
cargo run -p git-cli -- cat-file -s <object-or-rev>
cargo run -p git-cli -- cat-file -p <object-or-rev>
cargo run -p git-cli -- cat-file --batch
cargo run -p git-cli -- cat-file --batch=<format>
cargo run -p git-cli -- cat-file --batch-check
cargo run -p git-cli -- cat-file --batch-check=<format>
cargo run -p git-cli -- commit -m <message>
cargo run -p git-cli -- commit-tree <tree-id> -m <message>
cargo run -p git-cli -- ls-files [--stage|-s] [--cached|-c] [--others|-o] [--directory] [--no-empty-directory] [--deleted|-d] [--modified|-m] [--unmerged|-u] [--deduplicate] [--error-unmatch] [--full-name] [-z] [<path>...]
cargo run -p git-cli -- ls-files [--stage|-s] -- <path>...
cargo run -p git-cli -- ls-tree [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --name-only [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --name-status [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --object-only [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --long [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree -t [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree -d [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree -r [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree -r -t [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree -r -d [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --abbrev[=<n>] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --no-abbrev <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --full-name <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --no-full-name <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --full-tree <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --no-full-tree <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree --format <format> <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree <tree-ish> -- <path>...
cargo run -p git-cli -- ls-tree -r --name-only [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree -r --object-only [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- ls-tree -r --long [-z] <tree-ish> [<path>...]
cargo run -p git-cli -- log [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p git-cli -- log --oneline [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p git-cli -- log --reverse --oneline [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p git-cli -- log --pretty=oneline [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p git-cli -- log --format=oneline [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p git-cli -- log --format=<format> [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p git-cli -- log --pretty=format:<format> [-<n>|-n <n>|--max-count=<n>] [<commit-ish>]
cargo run -p git-cli -- rev-parse <rev>...
cargo run -p git-cli -- rev-parse --abbrev-ref <rev>...
cargo run -p git-cli -- rev-parse --symbolic-full-name <rev>...
cargo run -p git-cli -- rev-parse --verify [--quiet] [--end-of-options] <rev> [--]
cargo run -p git-cli -- rev-parse --short[=<n>] <rev>
cargo run -p git-cli -- rev-parse --path-format=<absolute|relative> <path-option>...
cargo run -p git-cli -- rev-parse --git-dir
cargo run -p git-cli -- rev-parse --absolute-git-dir
cargo run -p git-cli -- rev-parse --git-common-dir
cargo run -p git-cli -- rev-parse --git-path <path>
cargo run -p git-cli -- rev-parse --resolve-git-dir <path>
cargo run -p git-cli -- rev-parse --show-toplevel
cargo run -p git-cli -- rev-parse --show-prefix
cargo run -p git-cli -- rev-parse --show-cdup
cargo run -p git-cli -- rev-parse --show-superproject-working-tree
cargo run -p git-cli -- rev-parse --show-object-format
cargo run -p git-cli -- rev-parse --show-ref-format
cargo run -p git-cli -- rev-parse --local-env-vars
cargo run -p git-cli -- rev-parse --is-inside-work-tree
cargo run -p git-cli -- rev-parse --is-inside-git-dir
cargo run -p git-cli -- rev-parse --is-bare-repository
cargo run -p git-cli -- rev-parse --is-shallow-repository
cargo run -p git-cli -- write-tree
cargo run -p git-cli -- write-tree --prefix=<prefix>
cargo run -p git-cli -- write-tree --missing-ok
cargo run -p git-cli -- update-index [--add] [--remove|--force-remove] [--chmod=(+|-)x] [--cacheinfo <mode>,<object>,<path>] [--stdin|--index-info] [-z] <path>...
cargo run -p git-cli -- update-ref [--deref|--no-deref] refs/heads/main <object-id> [<old-object-id>]
cargo run -p git-cli -- update-ref -d <ref>
cargo run -p git-cli -- show-ref
cargo run -p git-cli -- show-ref --head
cargo run -p git-cli -- show-ref --heads
cargo run -p git-cli -- show-ref --branches
cargo run -p git-cli -- show-ref --tags
cargo run -p git-cli -- show-ref --branches --tags
cargo run -p git-cli -- show-ref --dereference [--tags]
cargo run -p git-cli -- show-ref --dereference --no-dereference [--tags]
cargo run -p git-cli -- show-ref --hash|--no-hash [--branches|--heads|--tags]
cargo run -p git-cli -- show-ref --abbrev[=<n>] [--no-abbrev] [--branches|--heads|--tags]
cargo run -p git-cli -- show-ref <pattern>...
cargo run -p git-cli -- show-ref -- <ref>...
cargo run -p git-cli -- show-ref --verify <ref>
cargo run -p git-cli -- show-ref --verify -- <ref>...
cargo run -p git-cli -- show-ref --verify --dereference <ref>
cargo run -p git-cli -- show-ref --verify --hash <ref>
cargo run -p git-cli -- show-ref --verify --quiet <ref>
cargo run -p git-cli -- show-ref --exists <ref>
cargo run -p git-cli -- show-ref --exclude-existing[=<pattern>]
cargo run -p git-cli -- symbolic-ref [--short] [--no-recurse] <name>
cargo run -p git-cli -- symbolic-ref [--quiet] <name>
cargo run -p git-cli -- symbolic-ref --delete <name>
cargo run -p git-cli -- symbolic-ref [-m <reason>] <name> <ref>
cargo run -p git-cli -- status --short
cargo run -p git-cli -- status --short [-z|--null]
cargo run -p git-cli -- status --short --branch
cargo run -p git-cli -- status --short --branch --no-branch
cargo run -p git-cli -- status --short [-u|-uall|-unormal|-uno|--untracked-files[=all|normal|no]]
cargo run -p git-cli -- status --porcelain
cargo run -p git-cli -- status --porcelain=1
cargo run -p git-cli -- status --porcelain [-z|--null]
cargo run -p git-cli -- status --porcelain --branch
cargo run -p git-cli -- status --porcelain [-u|-uall|-unormal|-uno|--untracked-files[=all|normal|no]]
cargo run -p git-cli -- status --porcelain=v1
cargo run -p git-cli -- status --porcelain=v1 [-z|--null]
cargo run -p git-cli -- status --porcelain=v1 --branch
cargo run -p git-cli -- status --porcelain=v1 [-u|-uall|-unormal|-uno|--untracked-files[=all|normal|no]]
cargo run -p git-cli -- stash list [--oneline|--format=%gd|--format=%gD|--format=%gs] [-<n>|-n <n>|--max-count=<n>]
cargo run -p git-cli -- stash clear
cargo run -p git-cli -- stash drop [-q|--quiet|--no-quiet] [stash@{<n>}]
cargo run -p git-cli -- stash show [-u|--include-untracked|--only-untracked] [--stat|--name-only|--name-status|-p|--patch|--oneline|--quiet] [stash@{<n>}]
cargo run -p git-cli -- stash store [-m <message>|--message=<message>] [-q|--quiet|--no-quiet] <commit>
cargo run -p git-cli -- tag [<name> [<target>]]
cargo run -p git-cli -- tag --list
cargo run -p git-cli -- tag --list <pattern>...
cargo run -p git-cli -- tag -l <pattern>...
cargo run -p git-cli -- tag --points-at <object-ish> [<pattern>...]
cargo run -p git-cli -- tag --contains <commit-ish> [<pattern>...]
cargo run -p git-cli -- tag --no-contains <commit-ish> [<pattern>...]
cargo run -p git-cli -- tag --merged [<commit-ish>]
cargo run -p git-cli -- tag --no-merged [<commit-ish>]
cargo run -p git-cli -- tag -f <name> [<target>]
cargo run -p git-cli -- tag -a <name> -m <message> [<target>]
cargo run -p git-cli -- tag -f -a <name> -m <message> [<target>]
cargo run -p git-cli -- tag -d <name>...
cargo run -p git-cli -- testkit hash-object
cargo run -p git-cli -- testkit hash-object-sha256
cargo run -p git-cli -- testkit loose-sha256
cargo run -p git-cli -- testkit config
cargo run -p git-cli -- testkit commit
cargo run -p git-cli -- testkit commit-tree
cargo run -p git-cli -- testkit branch
cargo run -p git-cli -- testkit branch-current
cargo run -p git-cli -- testkit branch-delete
cargo run -p git-cli -- testkit checkout
cargo run -p git-cli -- testkit tag
cargo run -p git-cli -- testkit tag-delete
cargo run -p git-cli -- testkit annotated-tag
cargo run -p git-cli -- testkit diff
cargo run -p git-cli -- testkit rev-parse
cargo run -p git-cli -- testkit rev-parse-parents
cargo run -p git-cli -- testkit rev-parse-peel
cargo run -p git-cli -- testkit rev-parse-object-format
cargo run -p git-cli -- testkit add-status
cargo run -p git-cli -- testkit index
cargo run -p git-cli -- testkit update-index
cargo run -p git-cli -- testkit ls-files
cargo run -p git-cli -- testkit update-ref-delete
cargo run -p git-cli -- testkit write-tree
cargo run -p git-cli -- testkit ls-tree
cargo run -p git-cli -- testkit cat-file
cargo run -p git-cli -- testkit log
cargo run -p git-cli -- testkit pack-read
cargo run -p git-cli -- testkit packed-odb
cargo run -p git-cli -- testkit pack-index
cargo run -p git-cli -- testkit pack-write
cargo run -p git-cli -- testkit refs
cargo run -p git-cli -- testkit show-ref
cargo run -p git-cli -- testkit show-ref-verify
cargo run -p git-cli -- testkit symbolic-ref
```
