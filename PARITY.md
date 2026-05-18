# Git Parity Tracker

Target: upstream Git 2.54.0.

## Implemented Initial Surface

- Workspace crate decomposition matching `GOAL.md`.
- Typed object identifiers with SHA-1 and SHA-256 object hashing, including
  `hash-object` filter/literally/path compatibility flags for raw-byte hashing.
- Git object encode/decode for blob, tree, commit, and tag envelopes.
- Tree entry parse/write support and `ls-tree`-style formatting for SHA-1
  repositories, including `--name-only`, `--name-status`, `--object-only`, `--long`,
  recursive `-r`, tree entry inclusion with `-t`, directories-only output with
  `-d`, `--abbrev[=<n>]` / `--no-abbrev`, `-z` output, and literal path
  filtering plus nested current-directory, `--full-name`, `--full-tree`, and
  their `--no-full-name` / `--no-full-tree` negations, and minimal `--format`
  placeholder path handling plus `--` pathspec separation and tree-ish peeling
  from commits and annotated tags.
- Commit parse/write support and minimal first-parent-style `log` formatting
  over parsed commit records, including upstream-style author identity display
  `-<n>` / `-n` / `--max-count` limiting, `--skip`, `--oneline`,
  `--pretty=oneline`, `--format=oneline`, minimal custom `--format`
  placeholders including commit, tree, parent, subject/body, author identity,
  committer identity, and author/committer timestamp fields, `--reverse`, and
  `--abbrev-commit` / `--no-abbrev-commit` plus `--abbrev[=<n>]` /
  `--no-abbrev` abbreviation control for covered commit-header/oneline/custom
  placeholder modes,
  `--topo-order`, `--date-order`, and `--author-date-order` ordering for
  covered single-revision walks,
  no-op `--sparse`, `--dense`, `--remove-empty`, `--unpacked`,
  `--full-history`, `--simplify-merges`, and `--show-pulls` for covered
  path-free walks,
  ref selectors (`--all`, `--branches[=<glob>]`, `--tags[=<glob>]`, and
  `--remotes[=<glob>]`, plus `--glob[=<glob>]`) with scoped `--exclude`
  and `--exclude-hidden[=<section>]` configured hidden-ref filtering for covered
  `--all` / `--glob` local ref walks,
  `--default <rev>` fallback when no explicit start revision is supplied,
  `--stdin` revision input for covered LF-delimited revision and `--not`
  toggle cases,
  multiple positive revisions, `^rev` / `--not` exclusions, and simple
  `A..B` / `A...B` ranges for covered log walks,
  `--parents` commit-header output for covered default/oneline modes,
  `--children` oneline header output, plus
  no-op parser compatibility for quiet, source, mailmap, encoding, and decoration
  disabling flags, and
  commit-ish peeling from annotated tags.
- Annotated tag object parse/write support and minimal `tag -a -m` /
  `tag -F` creation.
- Minimal `hash-object` support for stdin and path inputs, multiple path inputs,
  `--stdin-paths` / `--no-stdin-paths`, `--path` / `--no-path`, `-t` / `-t<type>`, `-w`,
  `--` path separator handling, no-value option rejection for covered flags,
  and explicit SHA-1/SHA-256 object-format hashing.
- Minimal `count-objects` support for loose-object counts and disk usage,
  including default output, `-H` / `--human-readable`, `-v` / `--verbose`,
  long option negations, combined short options, basic pack count / `size-pack` reporting,
  `prune-packable`, fanout-directory garbage warnings/counting, and verbose
  `objects/info/alternates` display, plus covered `GIT_OBJECT_DIRECTORY`
  primary object directory counting.
- Minimal `merge-base` support for two or more commit-ish arguments and `--all`,
  `--is-ancestor`, `--independent`, `--octopus`, and covered reflog-backed
  `--fork-point`, including ancestor inputs,
  no-common-history exit status, and ancestor-query exit status.
- Minimal read-only `reflog` / `reflog show` support for covered `HEAD`
  reflogs, including default oneline display, `--oneline`, `--format=%gs`,
  `--pretty=format:%gs`, `-<n>`, and `--max-count=<n>` output over the
  upstream-style reflog revision walk, plus `reflog exists <ref>` status
  checks for covered `HEAD`, full-ref, missing-ref, missing-argument, and
  extra-argument cases, and `reflog list` output/error parity for covered
  repositories with HEAD, branch, and tag reflogs, plus `reflog delete`
  parity for covered numeric `HEAD@{<n>}` selectors, dry-run, verbose,
  `--updateref` branch-ref movement, missing-selector, unknown-option, and
  invalid-selector cases, `reflog drop` parity for covered named refs, `HEAD`,
  multiple refs, `--all`, `--single-worktree`, missing-ref, and usage/error
  cases, `reflog write` parity for covered full-ref and `HEAD` appends,
  missing-ref reflog creation, zero-OID entries, empty messages, usage errors,
  invalid refnames, malformed OIDs, and missing-object errors, plus
  `reflog expire` parity for covered explicit `--expire` timestamps,
  `--expire=all` / `--expire=never`, `--expire-unreachable`
  reachable/unreachable pruning, dry-run, verbose, `--rewrite`, `--updateref`,
  `--all`, missing-ref, missing-value, unknown-option, and invalid-timestamp
  cases.
- Minimal `stash list` support over `refs/stash` reflogs, including default
  display, `--oneline`, custom `%H` / `%h` / `%T` / `%t` / `%P` / `%p` /
  `%s` / `%f` / `%e` / `%b` / `%B` / `%an` / `%ae` / `%al` /
  `%aN` / `%aE` / `%aL` / `%at` / `%ad` / `%ai` / `%aI` / `%as` / `%aD` /
  `%cn` / `%ce` / `%cl` / `%cN` / `%cE` / `%cL` / `%ct` / `%cd` / `%ci` /
  `%cI` / `%cs` / `%cD` / `%gd` / `%gD` / `%gn` / `%gN` / `%ge` / `%gE` /
  `%gs`, `%d` / `%D` decoration, `%m` mark, `%N` no-notes, unsigned-signature,
  source-literal, `%xNN`, and no-op color formats with `--date` mode support for
  default date placeholders and reflog selectors, object-name abbreviation controls
  including covered permissive `--abbrev=<value>` parsing,
  `--author` / `--committer` identity filtering, `--grep` filtering with
  case/fixed-string/extended/perl/all-match/invert toggles, reflog-grep
  filtering,
  numeric age filters plus covered explicit date cutoffs, accepted decoration
  and color-mode controls, mailmap toggles, notes, encoding, source, signature,
  patch-suppression, no-graph and tab-expansion toggles, ext-diff/textconv,
  full-diff, rename/copy detection, relative path,
  merge-diff suppression, diff-algorithm, whitespace, context, prefix,
  output-indicator, submodule, color-moved, pickaxe, ita, and rewrite no-op
  flags, no-walk, simple history walk toggles, and parent-count filters, covered
  history-limiting, ordering, filter reset, regexp short/long reset/value,
  notes/source/signature value rejections,
  `--reverse` rejection, `--skip` skipping, and `-<n>` / `-n` /
  `--max-count` limiting including negative
  count reset behavior and quiet toggles for covered stash entries and empty repositories,
  `stash clear` ref/reflog removal and covered empty/error cases, plus
  `stash drop` top/explicit-entry removal, quiet toggles, ref update/removal,
  and covered invalid selector/error cases, plus `stash show` default stat,
  raw, compact-summary, numstat, shortstat, summary, name-only, name-status,
  patch, quiet/no-quiet, exit-code, accepted ext-diff/textconv toggles,
  `--no-full-index` / `--no-compact-summary` reset behavior, covered
  no-value errors for visual and untracked-display toggles, diff-filter,
  explicit-stash, and untracked-display output, plus
  `stash store` ref/reflog updates with message options and covered invalid
  commit cases, plus covered `stash push` / `stash save --staged` behavior
  for staged-only changes, disjoint unstaged changes, and same-path unstaged
  cleanup failures after stash creation.
- Minimal `mktree` support for `ls-tree`-style stdin records, including
  default object validation, `--missing`, `-z`, `--batch`, tree-entry sorting,
  submodule commit entries, and upstream-readable tree object writes.
- Minimal read-only `submodule status` support for covered initialized local
  submodules, including default `git submodule` status behavior, `--cached`,
  exact path filters with `./` and trailing-slash normalization plus `--`
  separation, nested-current-directory path display, relative pathspecs, and
  directory pathspec selection with overlapping-filter deduplication,
  path-sorted output for covered multiple-submodule and filtered cases,
  quiet status suppression before and after `status`, accepted `--recursive`
  parsing and `--no-recursive` usage rejection, index gitlink OIDs, checked-out submodule
  `HEAD` OIDs, `+` prefixes when the worktree checkout differs from the index,
  `-` prefixes for covered deinitialized submodules, and simple matching-ref
  suffixes including exact tag preference and tag-name display, plus
  recursive read-only status for covered initialized nested submodules.
- Minimal `submodule init` support for covered local submodules, including
  `.gitmodules` path/url discovery, local `submodule.<name>.active`,
  `submodule.<name>.url`, and covered `submodule.<name>.update` config
  registration, pathspec selection, quiet registration suppression, and
  upstream-style registration stderr for absolute local URLs.
- Minimal `submodule sync` support for covered registered local submodules,
  including `.gitmodules` url discovery, local `submodule.<name>.url` updates,
  pathspec selection, quiet output suppression, accepted top-level `--recursive`
  parsing for non-nested fixtures, and `--no-recursive` usage rejection.
- Minimal `submodule set-url` support for covered local submodules, including
  exact path lookup in `.gitmodules`, `.gitmodules` URL updates, registered
  local config URL synchronization, quiet output suppression, missing-path fatal
  errors, and usage output for missing/extra arguments.
- Minimal `submodule set-branch` support for covered local submodules, including
  exact path lookup in `.gitmodules`, `--branch` / `-b` / `--branch=<name>`
  branch configuration, `--default` / `-d` branch removal, missing-path fatal
  errors, mutually exclusive option errors, and covered usage paths.
- Minimal `submodule foreach` support for covered initialized local submodules,
  including default entry banners, quiet suppression, empty commands, shell
  command execution in the submodule worktree, `name` / `sm_path` /
  `displaypath` / `sha1` / `toplevel` environment variables, failing-command
  fatal handling, and recursive traversal for covered nested-submodule cases.
- Minimal `submodule summary` support, including no-output handling for clean
  submodules, covered forward and reverse worktree-HEAD-vs-index summaries,
  cached HEAD-vs-index summaries, file/index-vs-worktree summaries,
  multi-commit forward summaries with `--summary-limit`, clean `--cached` /
  `--files` no-op parsing, commit/path argument parsing for covered cases,
  missing path no-op behavior, and covered usage/error paths.
- Minimal `submodule absorbgitdirs` support for covered local submodules,
  including no-op handling for already absorbed gitfiles, embedded `.git`
  directory migration into `.git/modules/<path>`, gitfile rewrite,
  `core.worktree` update, `--` path selection, quiet parsing, and missing-path
  errors.
- Minimal `submodule deinit` support for covered initialized local submodules,
  including `-f` / `--force` parsing, `--all` / explicit path selection, quiet
  output suppression, local `submodule.<name>` config removal, worktree
  directory clearing, covered untracked/modified worktree rejection without
  force, missing-path errors, and no-path fatal handling.
- Minimal `worktree add` support for covered local repositories, including
  explicit existing branch checkout, default new branch creation from the target
  path basename, detached `HEAD` worktrees, quiet output suppression, forced
  duplicate branch checkout, optional lock creation with reasons, linked
  worktree admin file/index creation, checked-out file materialization, branch
  already-in-use rejection, and covered usage/unknown-option errors.
- Minimal `worktree list` support for covered local repositories, including
  main and linked worktree enumeration, branch and detached-HEAD annotations,
  default aligned output, `--porcelain`, `--porcelain -z`, `--no-porcelain`,
  stale linked-worktree prunable annotations, `--no-expire` suppression, `-v`
  prunable reason output, linked-worktree locked annotations with and without
  reasons, and covered `-z` usage rejection without porcelain.
- Minimal `worktree prune` support for covered stale linked worktree admin
  directories, including `-n` / `--dry-run`, `-v` / `--verbose`, `--expire`,
  `--no-expire`, missing-expire-value errors, and actual stale admin directory
  removal, while skipping covered locked stale worktrees.
- Minimal `worktree lock` / `worktree unlock` support for covered linked
  worktrees, including optional `--reason` / `--reason=<reason>` lock reasons,
  lock file creation/removal, main-worktree rejection, missing/extra argument
  usage paths, double-lock and double-unlock fatal errors, and missing worktree
  fatal errors.
- Minimal `worktree remove` support for covered linked worktrees, including
  clean worktree and admin directory removal, dirty/untracked worktree rejection
  without force, single-force dirty removal, locked worktree rejection until
  double force, missing/extra argument usage paths, unknown-option errors,
  main-worktree rejection, and missing worktree fatal errors.
- Minimal `worktree move` support for covered linked worktrees, including clean
  and dirty worktree moves, existing-directory destinations, linked worktree
  admin `gitdir` updates, locked worktree rejection until double force,
  missing/extra argument usage paths, unknown-option errors, existing-file
  destination errors, main-worktree rejection, and missing worktree fatal
  errors.
- Minimal `worktree repair` support for covered linked worktrees moved outside
  Git, including explicit path repair from the main worktree, no-argument repair
  from inside a moved linked worktree, linked admin `gitdir` correction, invalid
  path errors, unknown-option usage, and linked-worktree admin directory
  discovery through `commondir`.
- Minimal `rev-list` support for commit-ish starts including annotated tags,
  linear-history traversal, `--max-count` / `-n` / `-<n>`, `--reverse`,
  `--skip`, default date-ready walk ordering, `--topo-order`, `--date-order`,
  `--author-date-order`, simple `--no-walk` /
  `--no-walk=sorted` / `--no-walk=unsorted` and `--do-walk`,
  `--default <rev>`,
  no-op `--sparse`, `--dense`, `--remove-empty`, `--unpacked`,
  `--full-history`, `--simplify-merges`, `--show-pulls`,
  `--exclude-promisor-objects`, and `--exclude-hidden=(fetch|receive|uploadpack)`
  configured hidden-ref filtering for covered `--all` / `--glob` selectors,
  `--parents`, `--children`, `-z`, `--count`, `--quiet`, `--all`, `--glob[=<glob>]`,
  `--branches[=<glob>]`, `--tags[=<glob>]`,
  `--remotes[=<glob>]`, `--exclude=<glob>` for covered scoped ref
  exclusions, `--first-parent`, `--merges`, `--no-merges`, `--min-parents=<n>`,
  `--max-parents=<n>` and their negations, `--left-right`, `--left-only`,
  `--right-only`,
  `--max-age[=<epoch>]`, `--min-age[=<epoch>]`, `--since` / `--after` /
  `--until` / `--before` for covered `@<epoch> <tz>` and explicit
  `YYYY-MM-DD[ T]HH:MM:SS <tz>` forms, identity filters
  (`--author[=<pattern>]` and `--committer[=<pattern>]`), message filters
  (`--grep[=<pattern>]`, `--all-match`, and `--invert-grep`), filter case
  folding (`-i` / `--regexp-ignore-case`), regexp mode flags (`-F` /
  `--fixed-strings`, `-E`, `--basic-regexp`, and `--extended-regexp`) for
  covered literal and simple regex cases, `--abbrev-commit`,
  `--abbrev=<n>`, `--no-abbrev`, `--no-abbrev-commit`,
  `--timestamp`,
  `--objects`, `--objects-edge`, `--objects-edge-aggressive`,
  `--no-filter`,
  `--filter=blob:none`, numeric `--filter=blob:limit=<n>`,
  numeric `--filter=tree:<depth>`, covered commit-walk
  `--filter=object:type=(blob|tree|commit)`, and direct/ref-selected
  annotated-tag starts include tag objects in covered `--objects` walks,
  `--object-names` / `--no-object-names`,
  `--disk-usage[=human]` for covered loose-object commit/object selections,
  `--boundary`, `--stdin`, `--header`, `--oneline`, `--pretty=oneline`,
  `--format=oneline`, `--pretty=short`, `--format=short`,
  minimal custom `--format=<format>` / `--pretty=format:<format>` placeholders including decoration, encoding, no-notes notes and covered no-op notes controls/aliases, covered no-op color controls, unsigned-signature, no-reflog reflog, revision marker, fixed date, covered `--date`-selected default date, non-mailmap identity, sanitized-subject, source-literal, and hex-escape placeholders,
  `^<rev>` exclusions, `--not` toggles, `--ignore-missing`, and `A..B` ranges.
  Covered pseudo-ref selectors honor `--not` toggles. Simple `A...B`
  symmetric ranges are expanded through the covered `merge-base --all` logic.
- Minimal `cat-file -e/-t/-s/-p` support for object IDs and revision/ref names,
  with raw-body pretty output for blobs, commits, and tag objects plus
  `ls-tree`-style pretty output for tree objects, and minimal
  `cat-file --batch` / `cat-file --batch-check` support for newline-delimited,
  `-z` NUL-input, and `-Z` NUL-input/output object/revision names, batch tuning no-op flags for
  covered stdin cases, mailmap no-op flags for covered no-mailmap cases,
  `--batch-all-objects` including covered `GIT_OBJECT_DIRECTORY`
  primary object directory enumeration and loose-object storage atoms,
  minimal `--batch-command`
  `info` / `contents` / buffered `flush`, plus minimal `--batch=<format>` and
  `--batch-check=<format>` / `--batch-command=<format>` placeholders
  for object name, type, size, disk size, delta base, and rest text.
- Git config parse/write support for sections, quoted subsections, scalar
  values, booleans, comments, and repository object-format detection.
- Minimal local `config` CLI support for `config <key>`, `config --get <key>`,
  `config --get-all <key>`, simple `config --get-regexp <pattern>` /
  `--name-only --get-regexp <pattern>` config-name matching, `config --list` /
  `config -l`, `config --list --name-only`, boolean read canonicalization
  through `config --bool` and `config --type=bool`, integer read
  canonicalization through `config --int` and `config --type=int`, typed
  `config --default <value> --get <key>` fallbacks, `-z` NUL-delimited output
  for covered get/get-all/list/get-regexp modes, `config <key> <value>`,
  `config --add <key> <value>`, `config --replace-all <key> <value>`,
  `config --unset <key>`, `config --unset-all <key>`,
  `config --rename-section <old> <new>`, and
  `config --remove-section <name>`.
- Repository layout initialization for SHA-1 and SHA-256 repository formats,
  including quiet mode, bare repositories, custom initial branch names, and
  upstream-style fresh/reinitialized stdout plus initial-branch reinit warnings.
- Index v2/v3/v4 entry parsing plus SHA-1 checksum validation, extension
  preservation, and byte-for-byte v2 write round-trips.
- Minimal SHA-1 `update-index` path support, including tracked path refresh,
  `-q`, `--add` / `--no-add`, `--remove` / `--no-remove`,
  `--force-remove` / `--no-force-remove`, `--chmod=(+|-)x`,
  `--assume-unchanged` / `--no-assume-unchanged`,
  `--skip-worktree` / `--no-skip-worktree`,
  simple `--fsmonitor-valid` / `--no-fsmonitor-valid` path marking,
  no-op parser compatibility for simple `--ignore-submodules`,
  `--replace`, `--unmerged`, `--ignore-skip-worktree-entries` path removal
  handling, `--info-only`,
  `--refresh` / `--really-refresh` including quiet dirty refresh and
  assume-unchanged handling,
  refresh-time `--ignore-missing` / `--no-ignore-missing`,
  `--verbose` / `--no-verbose`, and
  `--again` / `-g` for entries already differing from `HEAD`,
  no-op parser compatibility for ordinary-repository negated
  `--index-version`, `--split-index`, `--untracked-cache`,
  `--test-untracked-cache`, `--force-untracked-cache`, and `--fsmonitor`,
  `--test-untracked-cache` mtime probe reporting,
  `--fsmonitor` unset-configuration warning behavior,
  `--unresolve` ordinary no-resolve-undo no-op handling,
  no-input `--force-write-index` toggles, `--clear-resolve-undo`,
  `--index-version 2` / `--index-version 3` / `--index-version 4`,
  `--` path separator handling, `--show-index-version` / `--no-show-index-version`,
  `--cacheinfo`, `--index-info`, LF-delimited `--stdin`, and NUL-delimited
  `-z --stdin`, that writes loose blobs and checksummed index entries readable
  by upstream `git ls-files --stage`.
- Minimal `ls-files`, `ls-files -z`, `ls-files --stage` / `--no-stage`,
  `ls-files --stage -z`, `ls-files --cached` / `--no-cached`,
  `ls-files --others` / `--no-others`, and
  `ls-files --others --directory` / `--no-directory` /
  `--no-empty-directory` / `--empty-directory`,
  `ls-files --deleted` / `--no-deleted`, `ls-files --modified` /
  `--no-modified`, `ls-files --full-name`, `ls-files --unmerged` /
  `--no-unmerged`, `ls-files --deduplicate` / `--no-deduplicate`, and
  `ls-files --error-unmatch` / `--no-error-unmatch` support over SHA-1 index
  entries and worktree paths, `ls-files --others --exclude-standard`,
  `ls-files --others --ignored --exclude-standard`, and
  `ls-files --cached --ignored --exclude-standard` for root `.gitignore` and
  `.gitignore` files in the worktree, `.git/info/exclude`, configured
  `core.excludesFile`, and default XDG/HOME global excludes
  slash-aware wildcard/directory/bracket-class/negated patterns,
  trailing-space normalization, and escaped wildcard/comment/negation/trailing-space literals,
  plus `ls-files --others --exclude <pattern>` /
  `-x <pattern>` and `--exclude-from <file>` / `-X <file>` with `--ignored`
  combinations, plus `--exclude-per-directory <file>` for literal, slash-aware wildcard,
  directory, negated, and cached ignored exclude patterns, no-op parser compatibility for simple
  submodule/sparse/EOL/killed/resolve-undo/debug/abbrev negations,
  including short option aliases, nested-cwd filtering, prefix
  stripping, simple literal path arguments, and `--` pathspec separation.
- Minimal SHA-1 `write-tree` support that recursively writes tree objects from
  index entries, verifies indexed objects by default, supports `--missing-ok`
  / `--no-missing-ok` and `--prefix` / `--no-prefix`, and matches upstream
  `git write-tree` for covered cases.
- Minimal `commit-tree` support that writes canonical commit objects with
  explicit tree, parent, identity, `-m` message, and `-F` message-file data,
  including attached short-option forms, accepts `--no-gpg-sign`, and includes
  exact missing `-m` / `-F` / `-p` and tree-count errors.
- Minimal `commit -m` / `commit -F` support with quiet, no-verify, and signoff
  toggles that writes the current index tree, creates a commit, updates the
  symbolic `HEAD` target ref, appends a branch reflog, and matches upstream
  missing-message option errors.
- Minimal porcelain `add <path>` support backed by the index writer, including
    recursive directory path expansion plus tracked deletion staging for explicit
    file and directory pathspecs, `-n` / `--dry-run`, `-v` / `--verbose`,
    their `--no-*` negations, combined `-nv` / `-vn`, missing-pathspec errors,
    `--ignore-missing` dry-run pathspec checks, accepted `--sparse` and
    `--ignore-errors` no-op parsing for ordinary non-sparse repositories,
    `--ignore-removal` /
    `--no-all` staging additions and modifications while leaving deletions
    unstaged, `-u` / `--update` staging tracked modifications and
    deletions while leaving untracked files alone, `-A` / `--all` staging tracked
    modifications, tracked deletions, and untracked files, `--chmod=(+|-)x`
    with `--no-chmod`, and `add --pathspec-from-file` with LF or
    `--pathspec-file-nul` pathspec lists plus accepted negated forms,
  plus `status --short` / `status --porcelain` / `status --porcelain=1` /
  `status --porcelain=v1` / `status --porcelain=v2` support,
  including `-z` / `--null`, `--no-null`, `--branch` / `--no-branch`,
  combined `-sb` / `-bs`, and
  `-u` / `-uall` / `-unormal` / `-uno` / `--untracked-files[=all|normal|no]`,
  `--no-untracked-files` default-display reset, normal-mode untracked directory
  collapsing with `-uall` file expansion, plus root/nested `.gitignore`,
  `.git/info/exclude`, configured `core.excludesFile`, and default XDG/HOME global excludes
  wildcard/directory/bracket-class/negated filtering, including trailing-space normalization and
  escaped wildcard/comment/negation/trailing-space literals, for untracked files and `--ignored` /
  `--ignored=traditional` / `--ignored=matching` display for those ignored
  root patterns, for simple added, modified, deleted, and untracked file states.
- Minimal `check-ignore` support for path arguments, `--stdin`, `-z`,
  `--no-index`, `-q`, `-v` source/line/pattern reporting, and `-n` non-matching
  verbose rows, with covered long-option negations for stdin/index/quiet/verbose/non-matching,
  backed by standard root/nested `.gitignore`, `.git/info/exclude`, configured
  `core.excludesFile`, and default XDG/HOME global excludes matching with
  tracked-path suppression by default.
- Minimal `check-attr` support for explicit attributes, `--all` / `-a`,
  `--stdin`, `-z`, `--cached` / `--no-cached` with covered index-backed
  `.gitattributes` lookup, root and nested `.gitattributes`, `.git/info/attributes`,
  configured `core.attributesFile`, default XDG/HOME global attributes,
  `--source[=]<tree-ish>` tree-backed source attributes for covered committed
  `.gitattributes` cases, `--no-source` worktree-mode parser compatibility,
  set/unset/value states, builtin `binary` and custom `[attr]` macro expansion,
  basename and slash wildcard patterns, and unspecified attributes.
- Minimal `branch` support for listing local branches, `branch --list`
  patterns, `branch -r`, `branch -a`, remote/all `--list` patterns,
  local/remote/all `branch --points-at` plus order-sensitive `--no-points-at`,
  `branch --contains`, `branch --no-contains` including combined positive/negative filters and `--contains=` forms,
  `branch --merged`, `branch --no-merged` including combined positive/negative filters and `--merged=` forms, reporting the current branch with
  order-sensitive `--show-current` / `--no-show-current`, operand and option-terminator handling, and no-value errors,
  `--no-show-current` / `--no-list` / `--no-delete` list fallbacks, creating
  loose local branches from a start revision with covered `--quiet` /
  `--no-quiet`, option-terminator handling, and `--create-reflog` /
  `--no-create-reflog` parsing, plus
  `--track` / `-t` / `--track=direct` creation from local and remote-tracking
  branch starts, option-terminator handling, order-sensitive `--no-track`, and covered
  `--track=inherit` from local branch upstream config, plus
  `--no-recurse-submodules` creation no-op parsing and order-sensitive
  `--recurse-submodules` reset/error handling when submodule propagation is not enabled,
  deprecated `--set-upstream` fatal behavior and order-sensitive
  `--no-set-upstream` creation no-op parsing, and covered
  `--edit-description` / `--no-edit-description` editor-free no-op, reset,
  value-error, missing-branch, detached-HEAD, and multiple-branch error paths,
  upstream-style invalid branch-name errors for covered create and force-create paths,
  `--delete --no-delete` action cancellation for branch creation, deleting
  merged loose local branches with `branch -d`, order-sensitive `--quiet` /
  `--no-quiet` and `--force` / `--no-force` parsing for deletion,
  value-form rejection for delete-related no-value long options, clustered
  short deletion options such as `-dq`, `-Dq`, and `-dv`, plus
  option-terminator handling and order-sensitive `-a` / `-r` delete mode handling,
  remote-tracking branch deletion with `branch -r -d` / `-D` including
  clustered short forms, option-terminator handling, quiet/no-quiet ordering,
  missing-ref, no-arg, and full-ref-name errors,
  local and configured-remote upstream configuration with
  `branch -u` / `--set-upstream-to` and `--unset-upstream`, including
  option-terminator handling before branch operands and invalid explicit branch
  operand errors, missing invalid upstream names, and no-value long-option rejection,
  branch rename/copy actions with `-m` / `-M` / `--move` and
  `-c` / `-C` / `--copy`, including current-branch fallback, force-target
  checks, option-terminator handling, upstream-style invalid source/destination
  branch-name errors, no-value long-option rejection, branch config rename/copy,
  and reflog continuation entries,
  verbose branch listing with `-v` / `-vv` / `--verbose` /
  `--no-verbose` for covered local/remote/all and `--list` pattern forms,
  including option-terminator handling before list patterns,
  including abbreviated object names, commit subjects, and upstream
  ahead/behind annotations,
  force-resetting loose local branches with `branch --force` and
  order-sensitive `--no-force`, local branch `--no-format` cancellation and
  `--omit-empty` / `--no-omit-empty` formatting/display no-op parsing, and force-deleting loose local
  branches with missing/current branch error parity.
- Minimal clean-worktree `checkout <branch>` and
  `checkout -b <branch> [<start>]` / `checkout -B <branch> [<start>]`
  support, including `-q` / `--no-quiet`, accepted no-op parsing for covered
  ordinary branch-checkout flags, upstream-style switch messages on stderr,
  symbolic `HEAD` updates, target commit tree materialization, and index rewrites.
- Minimal clean-worktree `switch <branch>`, `switch -c <branch> [<start>]`,
  and `switch -C <branch> [<start>]` support over the same checkout backend,
  including `--create=<branch>`, `--force-create=<branch>`, `-q` /
  `--no-quiet`, and accepted no-op parsing for covered ordinary branch-switch
  flags.
- Minimal `restore <path>...` worktree restore support from the index,
  including file paths, directory pathspecs, and `--worktree` / `-W` no-op
  parsing, plus `restore --staged <path>...` index restore support from `HEAD`
  for modified, added, and deleted paths, `restore --source=HEAD <path>...`
  worktree restore from `HEAD` for modified and staged-added paths,
  `restore --source=<tree-ish>` / `restore -s <tree-ish>` worktree restore plus
  `restore --source=<tree-ish> --staged` index restore and
  `restore --source=<tree-ish> --staged --worktree` / `restore -s <tree-ish> -SW`
  index/worktree restore, and combined `restore -SW <path>...` /
  `restore --staged --worktree <path>...` index/worktree restore from `HEAD`,
  `restore --pathspec-from-file` with LF or `--pathspec-file-nul` pathspec
  lists, and accepted no-op parsing for covered ordinary-worktree flags.
- Minimal `clean` support for untracked files and untracked directories with
  `clean -n`, `clean -f`, `clean -d`, short-option combinations, covered
  default root/nested `.gitignore`, `.git/info/exclude`, configured
  `core.excludesFile`, and default XDG/HOME global excludes filtering and `-x`
  include-ignored behavior including slash-aware wildcard, bracket-class, and negated patterns,
  trailing-space normalization, and escaped wildcard/comment/negation/trailing-space literals, covered
  `-e` / `--exclude` literal and simple wildcard filtering, covered
  long-option negations, simple literal pathspec filtering including directory
  pathspecs, file pathspecs inside otherwise untracked directories, `--`
  pathspec separation, and default require-force refusal plus
  `clean.requireForce=false`.
- Minimal `rm <path>...` support for clean tracked files, removing paths from
  both the worktree and index, including `-q` quiet parsing, combined short
  option parsing for covered flags, their covered `--no-*` long-option
  negations, accepted `--sparse` no-op parsing for ordinary non-sparse
  repositories, and `-r` recursive
  directory path removal for tracked files, `-n` / `--dry-run` reporting without
  mutation, `--ignore-unmatch` missing-path success while removing matched paths,
  `--pathspec-from-file` with LF or `--pathspec-file-nul` pathspec lists
  requiring the upstream option pairing plus accepted negated forms, plus
  `--cached` index-only removal and `-f` forced worktree/index removal for
  modified tracked files.
- Minimal `mv <source> <destination>` / `mv <source>... <destination-directory>`
  support for tracked files, moving the
  worktree file and staging the index path update for clean and worktree-modified
  sources and tracked directories, plus `-f` destination overwrite parsing,
  `-n` / `--dry-run`, `-v` / `--verbose`, `-k` skip-error handling, combined
  short-option parsing, destination and dry-run error parity for missing
  directories, parents, bad/missing sources, untracked sources, and existing
  destinations, and accepted no-op parsing for covered upstream negated/sparse
  long options.
- Minimal mixed `reset [HEAD] <path>...` support for unstaging modified and
  added paths from `HEAD`, including `-q` quiet output suppression,
  `--no-quiet`, `--pathspec-from-file` with LF or `--pathspec-file-nul`
  pathspec lists, and the
  default "Unstaged changes after reset" summary for remaining worktree changes,
  `reset <tree-ish> [--] <path>...` index-only path reset from a source tree,
  `reset --mixed <commit>` branch/HEAD movement with index-only reset,
  `reset --soft <commit>` branch/HEAD-only movement with path rejection, and
  `reset --hard [<commit>]` branch/HEAD movement and worktree/index restore with
  upstream-style HEAD summary output and `-q`.
- Minimal `remote` support for listing configured remotes, order-sensitive
  top-level `remote -v` / `--verbose` / `--no-verbose` toggles,
  `remote add [-t <branch>|--track=<branch>] [-m <branch>]
  [--no-track] [--no-master] [--tags|--no-tags]
  [--mirror|--mirror=fetch|--mirror=push|--no-mirror] <name> <url>`
  with default, tracked-branch, tag-option, mirror, and remote-HEAD config/ref
  creation,
  `remote get-url [--push|--no-push] [--all|--no-all] <name>` including
  `url.<base>.insteadOf` and push `url.<base>.pushInsteadOf` rewriting, config-level
  `remote rename <old> <new>` with loose/packed local remote-tracking ref
  rewriting plus branch push/default remote config updates,
  `remote set-head <name> (-a|--auto|-d|--delete|<branch>)` including
  local/file remote auto-discovery, delete option forms before/after the remote
  name, and `--no-auto`/`--no-delete` reset behavior for local remote-tracking
  refs, `remote show [-n] [--] [<name>...]` with no-query local remote-tracking
  output plus local/file remote queried HEAD and tracked/new/stale branch
  status output, configured URLs, local pull branch config, and simple local
  push status output,
  `remote set-url [--push|--no-push]
  [--add|--no-add|--delete|--no-delete] <name> <url> [<oldurl>]` with
  old-URL regex matching and multi-match/delete errors,
  `remote set-branches [--add|--no-add]
  <name> <branch>...`, `remote prune [-n|--dry-run|--no-dry-run] [--] <name>`
  for local/file remote stale tracking refs, and `remote remove <name>` with
  local remote-tracking ref and branch/default remote config cleanup.
- Minimal `tag` support for listing, `tag --list` / `tag -l` patterns,
  `tag --points-at`, `tag --contains`, `tag --no-contains`, `tag --merged`,
  `tag --no-merged`, creating lightweight refs, creating annotated tag objects
  from message arguments or files with `--no-annotate` and `--no-sign`
  negation handling plus `--no-file` parsing,
  force-updating lightweight or annotated tags with `tag -f` and `--no-force`,
  and deleting loose tag refs including duplicate/missing tag error parity.
- Minimal `diff --name-status`, `diff --name-only`, HEAD comparison,
  `--exit-code`, `--quiet`, `-z`, `--raw` with raw object abbreviation controls,
  `--stat` including `--stat-count` truncation and no-op parsing for non-narrowing width aliases,
  `--compact-summary`, `--numstat`, `--shortstat`, `--summary`, `--diff-filter`,
  default full-file text patch output with `-p`, `-u`, `--patch`,
  `--patch-with-raw`, `--patch-with-stat`,
  order-sensitive `-s` / `--no-patch` and `--name-only` /
  `--name-status` precedence over raw/stat/numstat modes and combined
  `--summary` output with raw/stat/numstat/shortstat modes,
  `--src-prefix`, `--dst-prefix`,
  `--no-prefix`, `--default-prefix`, `--abbrev[=<n>]`, `--no-abbrev`, `--full-index`,
  `core.abbrev`, one-line hunk range formatting, and mode-change headers for
  simple add/delete/modify cases, plus default binary patch summaries for
  add/delete/modify and mode-only cases, and exact rename/copy similarity
  headers, including Git-style path quoting for patch headers,
  no-op parsing for `-a` / `--text`, `--no-ext-diff`, `--no-textconv`,
  non-coloring `--no-color` / `--color=never|auto`, and non-submodule
  `--ignore-submodules`, and cached/staged variants
  support for added, modified, and deleted paths known to the index/HEAD,
  exact rename detection for name output including `-M` / `--find-renames` /
  `--no-renames`, exact copy detection for name output including `-C` /
  `--find-copies` / `--find-copies-harder`, with simple literal pathspec
  filtering, excluding untracked files like upstream Git.
- Minimal `rev-parse` support for full object IDs, `HEAD`, direct ref names,
  local branch names, lightweight tag names, and commit parent suffixes
  (`^`, `^N`, `~`, and `~N`) plus peel suffixes (`^{}`, `^{object}`,
  `^{commit}`, `^{tree}`, and `^{tag}`), plus `--show-toplevel`,
  `--abbrev-ref`, `--symbolic-full-name`, `--verify`, `--short`, `--git-dir`,
  `--absolute-git-dir`, `--show-prefix`, `--show-cdup`, `--show-object-format`
  (including `storage`, `input`, and `output` selectors), `.git` file discovery
  for covered submodule worktrees, `--is-inside-work-tree`,
  `--show-superproject-working-tree` for covered submodule and non-submodule repositories,
  `--is-inside-git-dir`, `--is-bare-repository`, `--is-shallow-repository`,
  and `--sq-quote`.
- Loose object zlib read/write for SHA-1 and SHA-256 repositories.
- Basic SHA-1/SHA-256 pack v2/v3 read support for undeltified
  commit/tree/blob/tag entries and resolved `ofs-delta` / `ref-delta`
  objects with trailer checksum validation, plus explicit thin `ref-delta`
  parsing when callers provide external base object lookup.
- Pack index v1 read support plus pack index v2 read support for SHA-1/SHA-256
  repositories, including fanout, CRC for v2, small/large offsets for v2,
  trailer checksums, and object lookup.
- Pack reverse-index v1 read/write support for SHA-1/SHA-256 repositories, including
  RIDX header/hash validation, index-position permutation validation, and
  trailer checksums.
- Pack mtimes v1 read/write support for SHA-1/SHA-256 repositories, including MTME
  header/hash validation, object-count validation, and trailer checksums.
- Multi-pack-index header and chunk-directory parsing for SHA-1/SHA-256
  repositories, including version/hash validation, base-count rejection,
  monotonic chunk offsets, terminator validation, trailer checksums, and PNAM
  packfile-name parsing with v1 sort validation, plus OIDF/OIDL/OOFF object
  table parsing, optional LOFF large-offset resolution, fanout validation, and
  object lookup by ID, plus optional RIDX pseudo-pack order and BTMP
  bitmapped-pack table parsing, and basic MIDX writing for PNAM/OIDF/OIDL/OOFF
  with LOFF large offsets plus filesystem ODB reads through
  `objects/pack/multi-pack-index` and CLI `multi-pack-index write` plus
  `multi-pack-index write --stdin-packs`, structural `multi-pack-index verify`, and quiet baseline
  `multi-pack-index expire` repository/object-dir flows, including covered
  `GIT_OBJECT_DIRECTORY` defaults, verified by upstream Git.
- Pack v2 and pack index v2 write support for undeltified SHA-1/SHA-256
  objects, plus bounded SHA-1/SHA-256 `ref-delta` / `ofs-delta` pack
  generation against the previous same-type object when the encoded delta is
  smaller than the full object.
- Filesystem object database reads loose objects first, then SHA-1/SHA-256 pack/index
  pairs from `.git/objects/pack`, including resolved delta entries.
- Loose, packed, and symbolic ref parsing plus canonical sorted packed-ref
  writing, loose direct-ref compaction into packed refs with optional pruning
  plus caller-supplied or automatic peeled IDs, minimal `pack-refs` CLI
  coverage for default tag packing / `--all` / pruning toggles plus covered
  `--include` / `--exclude` pattern filtering and negation resets, and
  in-memory and filesystem-backed loose ref transactions and loose and packed
  direct ref deletion, including direct detached `HEAD` deletion.
- Loose ref `.lock` writes, packed-ref `.lock` writes, packed-ref lookup, ref listing with heads/tags
  filters including `--branches` and filter negations, `show-ref --head`,
  `show-ref --hash` / `-s` / `-s<n>` including compact `-d`/`-q`/`-s` clusters /
  `--no-hash` / `--abbrev` / `--no-abbrev` formatting,
  suffix-component pattern filters, `--` ref separation, annotated tag
  dereferencing with `show-ref -d` / `--no-dereference`, unmatched filter exit
  status, `show-ref --verify` / `--no-verify`, `show-ref --exists` /
  `--no-exists` including `HEAD` and arity errors, symbolic ref target resolution for listed and verified refs,
  `--quiet` / `--no-quiet`, `show-ref
  --exclude-existing[=<pattern>]`, and
  reflog append/read helpers plus timestamp-cutoff expiration for SHA-1
  filesystem-backed repositories.
- Minimal `for-each-ref` support with default formatting, `--count`,
  `--sort=refname` / `--sort=-refname`, `--sort=objectname` /
  `--sort=-objectname`, `--sort=objecttype` / `--sort=-objecttype`,
  `--sort=objectsize` / `--sort=-objectsize`, loose `objectsize:disk` sort
  keys, upstream, push, symbolic-ref target, worktree path, and annotated tag
  metadata sort keys, subject/content-subject, body/content-body, content-size, and peeled message sort keys,
  peeled tag object name/type/size/disk-size/deltabase/raw-size sort keys,
  commit and peeled commit tree/parent/parent-count sort keys, author/committer/tagger/creator
  date and peeled date sort keys, identity and peeled identity sort keys,
  version-aware refname sort keys, multiple sort key
  precedence, `--ignore-case`, `--points-at`, `--contains`,
  `--no-contains`, `--merged`, `--no-merged`, prefix filters, and `--format`
  / `--format <format>`, `--color` / `--no-color`, `--stdin` / `--no-stdin`, `--start-after`, `--exclude`, `--omit-empty` /
  `--no-count`, `--no-sort`, `--no-start-after`, `--no-exclude`, `--no-omit-empty`, `--include-root-refs` / `--no-include-root-refs`, atoms
  for percent-hex and color escapes including attributes, reset attributes, foreground/background colors, and bright colors, current-branch marker, ref name, short/strip/lstrip/rstrip ref name, symbolic ref target,
  basic `--shell` / `-s` / `--python` / `--perl` / `-p` / `--tcl` placeholder quoting,
  upstream ref target with short/strip/lstrip/rstrip modifiers, upstream remote metadata
  through direct and wildcard fetch refspec mapping,
  bracketed/unbracketed tracking status, tracking status shorthand, ahead/behind counts against
  a named revision, push ref target with short/strip/lstrip/rstrip modifiers and remote metadata,
  push tracking status atoms, abbreviated object name with
  `core.abbrev` / explicit widths including invalid-width rejection and
  object-database uniqueness expansion, deltabase, peeled tag object name/type/size/disk size/deltabase/raw body,
  loose object disk size, raw object body/size, object size, object type,
  checked-out worktree path, subject,
  contents, contents size, contents lines, contents subject/body, peeled contents/subject/body, body, author/committer/tagger
  identity fields including email trim/localpart modifiers, peeled author/committer/creator identities,
  creator identity, raw author/committer/tagger/creator dates and peeled author/committer/creator dates,
  default and `unix`/`short`/`iso`/`iso8601`/`iso8601-strict`/`rfc2822`
  author/committer/tagger/creator dates, commit tree/parent/parent-count atoms, annotated
  tag name/type/object atoms, peeled commit tree/parent/parent-count atoms, and literal `%%`.
- In-memory object database with validation and write-once semantics.
- Basic commit graph traversal over a supplied object reader with parsed commit
  records.
- Commit-graph file v1 parsing for SHA-1/SHA-256 repositories, including
  header/hash validation, chunk-table validation, trailer checksums,
  OIDF/OIDL fanout and sorted-OID validation, CDAT root-tree/parent/generation
  and commit-time parsing, EDGE octopus-parent expansion, BASE graph hash
  parsing, GDA2/GDO2 corrected commit-date offset parsing with overflow
  validation, BIDX/BDAT changed-path Bloom filter metadata parsing, object
  lookup by commit ID, and revision parent-suffix resolution accelerated by
  single-file `objects/info/commit-graph` parent data when present, plus CLI
  `commit-graph verify` parsing of single-file commit-graphs generated by
  upstream Git and split commit-graph chains, minimal no-selector
  `commit-graph write` no-op behavior, and minimal `commit-graph write
  --reachable` single-file graph generation, including covered
  `GIT_OBJECT_DIRECTORY` defaults, verified by upstream Git.
- Bundle v2/v3 header parsing for SHA-1/SHA-256 repositories, including
  v3 capability parsing, `object-format` capability handling, prerequisites,
  advertised references, header/pack separation, raw pack payload retention,
  pack payload parsing with the bundle object format, and prerequisite
  availability verification through the object reader abstraction, plus object
  import from bundle pack payloads through the object writer abstraction,
  bundle file writing for prepared pack payloads, and bundle advertised-ref
  application through the ref transaction path, with CLI `bundle create --all`,
  covered `bundle create --all <rev>` / `--all ^<rev>` combinations,
  incremental `bundle create <rev> ^<rev>`, `bundle verify`,
  `bundle list-heads`, `bundle unbundle`, and quiet `fetch <bundle>` /
  `fetch <bundle> <src>[:<dst>]` interop against upstream Git including
  `FETCH_HEAD` writing, lightweight tag auto-follow for updating refspecs, and
  `--no-tags` auto-follow suppression plus `--tags` full tag import with
  upstream-compatible `FETCH_HEAD` merge markers and bundle tag ordering,
  repeated-fetch suppression for already-present auto-followed tags,
  plus quiet local-repository `fetch <path>` default `HEAD` FETCH_HEAD import
  and explicit `<src>:<dst>` refspec object/ref import, including `--no-tags`
  suppression and `--tags` full tag import, and configured local
  remote `fetch <name>` / default `fetch` using `remote.<name>.fetch`,
  `remote.<name>.tagOpt` configured `--tags` / `--no-tags` behavior with CLI
  tag-option override,
  configured local-remote `fetch --prune` stale remote-tracking ref pruning,
  `remote.<name>.prune` / `fetch.prune` default pruning with CLI
  `--no-prune` override and remote-specific precedence,
  configured local-remote `fetch --dry-run` / `--no-dry-run` ref-update
  suppression while retaining upstream-compatible object import,
  configured local-remote `fetch --append` / `--no-append` `FETCH_HEAD`
  append/overwrite behavior,
  configured local-remote `fetch --no-write-fetch-head` /
  `--write-fetch-head` `FETCH_HEAD` suppression and restoration,
  plus local-repository fetch through direct file URLs and
  `url.<base>.insteadOf` rewriting,
  plus local-path, file URL, `url.<base>.insteadOf` rewriting, and configured-local-remote `ls-remote` parity for covered
  default output, branch/tag/ref filtering, symref output, pattern matching,
  `--sort=refname`, `--sort=-refname`, version refname sorting, objectname
  sorting including the upstream outside-repository fatal path, objecttype,
  objectsize, objectsize:disk, authordate, committerdate, taggerdate, and
  creatordate sorting with local-object requirements and missing-object
  failures, bad sort keys, `--exit-code`, and `--get-url`.
- Typed pkt-line data/control-frame encoding and parsing for protocol plumbing,
  including flush, delimiter, and response-end packets plus incremental
  `Read`/`Write` helpers, bounded reads through flush/response-end, protocol
  `ERR` line parse/encode/read/write helpers, git service request,
  service announcement, and discovery response parse/encode/read/write
  helpers for upload-pack, receive-pack, and upload-archive with
  host/protocol extra parameters, smart HTTP info/refs and RPC path/content-type
  helpers, dumb HTTP `info/refs` parse/encode/read/write helpers for plain and
  peeled refs, dumb HTTP `objects/info/http-alternates` path and
  parse/encode/read/write helpers, dumb HTTP `objects/info/packs` path and
  parse/encode/read/write helpers, dumb HTTP loose-object, packfile, and pack-index
  path helpers, remote URL classification for local, file, SSH, scp-like SSH,
  git, HTTP, and HTTPS remotes, refspec parse/encode/source-mapping helpers for
  forced, negative, delete, direct, and wildcard refspec forms, FETCH_HEAD
  parse/encode/read/write helpers used by bundle fetch, fetch ref-update planning
  from advertised refs and refspecs including negative refspec filtering and tag
  auto-follow, push receive-pack command planning for create/update/delete,
  matching, and wildcard refspecs, receive-pack push request construction with
  advertised capability negotiation and push-option sections, Git credential
  protocol parse/encode/read/write helpers with extension attribute
  preservation plus Basic and Bearer HTTP authorization value builders, SSH
  service command construction helpers, plus colon-separated `Git-Protocol` header parse/encode helpers, and
  validated frame writes, plus capability token parsing/encoding, sideband
  channel packet parse/encode/read/write stream helpers for pack data,
  progress, and fatal channels plus sideband demux into
  pack bytes/progress/fatal errors, v0/v1
  upload-archive argument request and ACK/NACK sideband response
  parse/encode/read/write/demux helpers,
  advertised-ref payload and stream parse/encode/read/write helpers including
  protocol v1 version lines and shallow advertised refs, upload-pack
  advertised feature parse/encode and request validation helpers, upload-pack
  request parse/encode/read/write helpers, upload-pack shallow-update
  parse/encode/read/write helpers, upload-pack negotiation `have`/`done`
  parse/encode/read/write helpers, upload-pack ACK/NAK parse/encode/read/write
  helpers, upload-pack packfile response ACK/NAK plus sideband/raw-pack
  parse/encode/read/write helpers, receive-pack ref-update request and
  complete push request command/push-option/raw-pack stream parse/encode/read/write helpers,
  receive-pack advertised feature parse/encode and push request validation
  helpers, receive-pack push-options parse/encode/read/write helpers, receive-pack
  report-status and report-status-v2 parse/encode/read/write helpers, and protocol v2
  capability advertisement parse/encode/read/write helpers with object-format
  negotiation, command capability validation, typed command classification, and
  session request read/classify helpers, command
  request parse/encode/read/write helpers plus empty-request session
  termination handling and typed command-option helpers for agent,
  object-format, repeated server-option, and preserved extra capabilities, and
  typed protocol v2 `ls-refs`
  feature advertisement parsing/encoding and unborn-gated request validation,
  request parse/encode/read/write, response parse/encode/read/write,
  stateless response-end read/write, and request/response exchange helpers including
  peel, symrefs, unborn HEAD, ref-prefix arguments, peeled refs, symref targets,
  and unknown attribute preservation, plus typed protocol v2 `fetch`
  feature advertisement parsing/encoding and feature-gated request validation,
  fetch command validation, request parse/encode/read/write helpers for wants, want-refs, haves, shallow/deepen
  negotiation, filters, packfile URI advertisement, pack options, wait-for-done,
  and done, plus typed protocol v2 `fetch` response parse/encode/read/write,
  stateless response-end read/write, and request/response exchange helpers for
  acknowledgments, shallow-info, wanted-refs, typed packfile-uris with pack
  hashes, raw packfile
  payload packets, sideband-all response parse/encode/read/write helpers,
  packfile sideband demux, and unknown section preservation, plus typed protocol
  v2 `object-info` size request/response parse/encode/read/write/exchange
  helpers and command classification.
- CLI entry points for `init`, `hash-object`, `hash-object -w`, `cat-file`,
  leading global `-C <path>` directory changes and `-c init.defaultBranch=<name>`
  init config overrides plus `-c core.abbrev=<value>` for covered abbreviation
  defaults and `-c core.logAllRefUpdates=<value>` for covered update-ref
  reflog policy, `--config-env[=]<name>=<envvar>` for covered abbreviation
  defaults, `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` /
  `GIT_CONFIG_VALUE_<n>` for covered abbreviation defaults and parse errors,
  upstream-compatible `version` / global `--version` / `-v` output,
  and no-op parsing for covered pager/advice/lock/replacement global flags,
  plus covered global `--git-dir[=<path>]` and
  `--work-tree[=<path>]` repository path overrides and `--bare` repository
  mode for covered `rev-parse` queries, covered `GIT_DIR` /
  `GIT_WORK_TREE` repository environment handling for `rev-parse`, and
  covered `GIT_INDEX_FILE` alternate-index handling for `update-index`,
  `ls-files`, `write-tree`, and `check-attr --cached`, plus covered `GIT_OBJECT_DIRECTORY`
  alternate-object-directory handling for `hash-object -w`, `cat-file`,
  `commit-graph`, and `multi-pack-index`,
  and covered `objects/info/alternates` plus `GIT_ALTERNATE_OBJECT_DIRECTORIES`
  object lookup for `cat-file`,
  `add`, `branch`, `branch -d`, `checkout`, `checkout -b`, `checkout -B`, `clean`, `config`, `commit`, `commit-tree`,
  `diff --name-status`, `diff --name-only`, HEAD diff comparison, diff
  `--exit-code` / `--quiet`, diff `-z`, diff `--raw` with abbreviation controls,
  diff `--stat` including `--stat-count` truncation and non-narrowing width aliases, diff
  `--compact-summary`, diff `--numstat`, diff `--shortstat`, diff `--summary`, diff `--diff-filter`,
  no-op accepted diff options for text/ext-diff/textconv/submodule toggles,
  path-limited diff, exact rename/copy name output, cached/staged diff variants,
  `ls-files`, `ls-tree`, `log`, `mktree`, `rev-list`, `commit-graph verify`, `commit-graph write --reachable`,
  `multi-pack-index write`, `restore`, `rev-parse`, `write-tree`, `update-index`,
  `update-ref`, `update-ref [<old-oid>]`, `update-ref [--deref|--no-deref]`,
  `update-ref -d` including optional old-OID checks, `update-ref --` ref separation,
  update-ref symbolic-ref dereferencing controls, update-ref new-object
  validation for nonexistent objects and non-commit `HEAD` / branch writes,
  zero new-OID delete behavior for normal and stdin updates, and stdin
  `create` zero new-OID errors,
  line-oriented `update-ref --stdin` basic update/create/delete/verify commands,
  implicit line-oriented `update-ref --stdin` transaction rollback for covered
  direct-ref failures and duplicate direct/symbolic ref update rejection,
  implicit `prepare` rollback on EOF plus covered `prepare`/`commit` closure,
  updates before a later explicit `start`, and nested `start` rejection,
  closed transaction rejection after `commit` / `abort` plus reopening with
  `start`, and no-deref `HEAD` rollback on abort/failure,
  line-oriented `update-ref --stdin` successful `abort` including covered
  rollback for direct ref updates and duplicate direct-ref update rejection
  with rollback at `prepare`, explicit transaction EOF rollback, prepared
  transaction close-only enforcement,
  NUL-delimited `update-ref --stdin -z` update/create/delete/verify commands
  with implicit transaction duplicate direct-ref update rejection,
  nested `start` rejection and closed transaction rejection after `commit`,
  plus successful start/prepare/commit/abort wrappers including covered abort
  rollback for direct ref updates and duplicate direct-ref update rejection
  with rollback at `commit`, and prepared transaction close-only enforcement,
  successful `update-ref --stdin --batch-updates` / `-0` parsing for covered
  line and NUL-delimited stdin updates plus no-stdin fatal behavior and
  covered per-ref rejection continuation for old-OID/exists/missing failures
  and invalid new object values,
  line and NUL-delimited `update-ref --stdin` `symref-create` plus covered
  `symref-update` no-old, `ref <old-target>`, and `oid <old-oid>` checks,
  no-deref `symref-verify` success/missing/mismatch/deref-mode errors and
  no-deref `symref-delete` matching/missing/mismatch/deref-mode behavior,
  `update-ref --stdin` `option no-deref` toggling and unknown option errors,
  update-ref reflog creation policy,
  `show-ref`, `show-ref --verify`,
  `show-ref --exists`, `switch`, `symbolic-ref`, `status --short`, `status --porcelain`,
  `status --porcelain=1`, `status --porcelain=v1`, `status --porcelain=v2`, status `-z` / `--null`,
  status `--no-null`, status `--branch` / `--no-branch`, status `-u` /
  `--untracked-files`, status `--untracked-files=no`, `tag`, `tag -f`, and `testkit`
  parity checks, including SHA-256 pack-read, pack-index, pack-write,
  packed-ODB checks, SHA-1/SHA-256 deltified pack-read checks, thin pack-read
  checks with external bases, SHA-1/SHA-256 Rust-written deltified pack checks,
  and SHA-1/SHA-256 packed-ODB reads from deltified upstream packs.

## Major Gaps

- Broad delta selection, broad thin-pack workflows, and advanced pack generation
  are not implemented.
- Broader SHA-256 repository interop beyond loose objects and undeltified
  pack/index read/write is not implemented.
- Broad packed-ref transactions, broad reflog expiration policy/gc, and reftable
  refs are not implemented.
- Full worktree status/add/checkout flows, diff/merge, sequencer, transport,
  and porcelain commands are not implemented.
- Upstream Git test script import is not implemented; the current harness covers
  SHA-1/SHA-256 object hashing, upstream repository config parsing,
  upstream index byte-for-byte round-trips, Rust-written `update-index`
  tracked refresh plus `--add` / `--no-add`, `--remove` / `--no-remove`, and
  `--force-remove` / `--no-force-remove`
  interoperability with upstream `git ls-files --stage`, Rust `ls-files`,
  `ls-files -z`, `ls-files --stage`, `ls-files --stage -z`, `ls-files --others`,
  `ls-files --others -z`, `ls-files --stage --others`, `ls-files --deleted`,
  `ls-files --deleted -z`, `ls-files --stage --deleted`, and
  `ls-files --others --deleted` / `--stage --others --deleted`, plus
  `ls-files --cached`, `ls-files --cached -z`, `ls-files --cached --others`,
  `ls-files --modified`, `ls-files --modified -z`, `ls-files --stage --modified`,
  short option aliases, `ls-files --deduplicate --deleted --modified`,
  `ls-files --error-unmatch`, and combined cached/deleted/modified formatting
  parity over parsed index entries and worktree paths, plus CLI smoke coverage
  for nested-cwd prefix behavior,
  `ls-files --full-name`, literal `ls-files <path>` filters, and `--` pathspec
  separation, plus `ls-files` quoting for space, quote, and tab paths in
  non-NUL cached/staged/modified/deleted/other output, recursive Rust-written `write-tree` parity, `update-ref` old-OID
  checks, symbolic-ref update/delete dereferencing with `--deref` / `--no-deref`, `--` ref separation,
  attached `-m<reason>` parsing, covered `--create-reflog` creation and default
  tag/branch reflog policy, `update-ref -d` optional old-OID checks,
  no-deref direct `HEAD` deletion, and missing-ref
  success behavior, and line-oriented `update-ref --stdin` update/create/delete/verify
  plus implicit transaction rollback for covered direct-ref failures and
  duplicate direct/symbolic ref update rejection, plus successful
  implicit `prepare` rollback on EOF, `prepare`/`commit` closure,
  updates before a later explicit `start`, nested `start` rejection,
  closed transaction rejection after `commit` / `abort`, reopening with
  `start`, no-deref `HEAD` rollback on abort/failure,
  start/prepare/commit/abort wrappers and covered direct-ref abort rollback
  plus duplicate direct-ref update rejection at `prepare`, explicit
  transaction EOF rollback, and prepared transaction close-only enforcement,
  NUL-delimited
  `update-ref --stdin -z` update/create/delete/verify and
  implicit transaction duplicate direct-ref update rejection plus nested
  `start` rejection and closed transaction rejection after `commit`,
  start/prepare/commit/abort wrappers with covered direct-ref abort rollback,
  duplicate direct-ref update rejection at `commit`, prepared transaction
  close-only enforcement, successful
  `--batch-updates` / `-0`
  parsing for covered line/NUL updates, no-stdin fatal behavior, and covered
  old-OID/exists/missing per-ref rejection continuation, and
  line/NUL `symref-create`, covered `symref-update` no-old,
  `ref <old-target>`, and `oid <old-oid>` checks, plus covered no-deref `symref-verify`
  success/missing/mismatch/deref-mode behavior plus covered no-deref
  `symref-delete` matching/missing/mismatch/deref-mode behavior, and
  `option no-deref`,
  loose and packed ref deletion observed by upstream `git show-ref`,
  deterministic `commit-tree` parity,
  minimal `commit -m` interop readable by upstream `git rev-parse` and
  `git log`, commit empty-message rejection and
  `--allow-empty-message` / `--no-allow-empty-message` toggle parity for
  covered `-m` / `-F` messages, clean-index rejection and
  `--allow-empty` / `--no-allow-empty` toggle parity, covered raw
  `--author` and `--date` object parity, covered `-a` / `--all` tracked
  modification/deletion object parity, covered `-C` / `--reuse-message`
  object parity including author/date overrides, covered `-c` /
  `--reedit-message` no-editor-change object parity, covered
  reuse/reedit negation reset parsing, covered `--reset-author` reuse-message
  object parity and `--no-reset-author` reset/no-op parsing, covered `--amend`
  object parity including no-edit, reset-author, and author/date override cases,
  covered simple `--fixup <commit>` / `--fixup=<commit>` object parity
  including appended `-m` body text, `amend:` and `reword:` editor-noop object
  parity, and covered `-C` / `-c` / `-F` conflict errors,
  covered simple `--squash <commit>` / `--squash=<commit>` object parity
  including editor-noop, `-m`, `-F`, and `-C` body reuse cases plus
  `--fixup` conflict errors, covered commit long-option reset parsing for
  message/author/date/fixup/squash values,
  covered `--trailer` insertion for token/value forms, trailer-only message
  creation, `--no-trailer` reset behavior, and signoff/trailer ordering,
  plus covered commit no-op/reset parsing and value rejection for quiet/verify/post-rewrite/status/verbose/untracked-files/include/only/edit/branch/template/file toggles, covered
  `-S` / `--gpg-sign[=<key>]` reset parsing with `--no-gpg-sign`, covered
  commit `--short` / `--porcelain` / `--long` / `-z` / `--null` status-preview output and reset/value-error parity,
  including covered `--dry-run` default/long/short/porcelain/null preview combinations,
  reset/no-op parsing and value rejection for dry-run/long/ahead-behind/interactive/patch display toggles,
  `stash list` display over `refs/stash` reflogs including default,
  `--oneline`, custom `%H` / `%h` / `%T` / `%t` / `%P` / `%p` / `%s` /
  `%f` / `%e` / `%b` / `%B` / `%an` / `%ae` / `%al` / `%aN` /
  `%aE` / `%aL` / `%at` / `%ad` / `%ai` / `%aI` / `%as` / `%aD` / `%cn` /
  `%ce` / `%cl` / `%cN` / `%cE` / `%cL` / `%ct` / `%cd` / `%ci` / `%cI` /
  `%cs` / `%cD` / `%gd` / `%gD` / `%gn` / `%gN` / `%ge` / `%gE` / `%gs`,
  `%d` / `%D` decoration, `%m` mark, `%N` no-notes, unsigned-signature,
  source-literal, `%xNN`, and no-op color formats with `--date` mode support for default date placeholders and reflog selectors,
  object-name abbreviation controls including covered permissive
  `--abbrev=<value>` parsing,
  `--author` / `--committer` identity filtering, `--grep` filtering with
  case/fixed-string/extended/perl/all-match/invert toggles, reflog-grep
  filtering,
  numeric age filters plus covered explicit date cutoffs, accepted decoration
  and color-mode controls, mailmap toggles, notes, encoding, source, signature,
  patch-suppression, no-graph and tab-expansion toggles, ext-diff/textconv,
  full-diff, rename/copy detection, relative path,
  merge-diff suppression, diff-algorithm, whitespace, context, prefix,
  output-indicator, submodule, color-moved, pickaxe, ita, and rewrite no-op
  flags, no-walk, simple history walk toggles, and parent-count filters, covered
  history-limiting, ordering, filter reset, regexp short/long reset/value,
  notes/source/signature value rejections,
  `--reverse` rejection, `--skip` skipping, quiet toggles, and count limiting, plus
  `stash clear` ref/reflog removal and `stash drop` top/explicit-entry removal,
  `stash show` default raw/stat/compact-summary/numstat/shortstat/summary/name/name-status/patch/quiet/exit-code/diff-filter/untracked output plus covered visual/untracked-toggle no-value errors, `stash store` ref/reflog writes,
  `stash push` / `stash save --staged` creation and same-path unstaged cleanup
  failure parity,
  covered `-U` / `--unified` and `--inter-hunk-context` value parsing and
  patch/interactive requirement errors,
  covered commit `--pathspec-from-file` and `--pathspec-file-nul` parser
  errors and reset parsing,
  `-t` template alias and bare option terminator parity, covered
  `--cleanup` object parity including `--no-cleanup`, and `--template` value errors, Rust add/status interop with upstream `git status --short`,
  `git status --porcelain`, `git status --porcelain=1`,
  `git status --porcelain=v1`, `git status --porcelain=v2`
  over unborn and committed tracked-change fixtures, normal long status output
  for covered unborn-clean/staged/unstaged/untracked fixtures, plus status quoting for
  space, quote, and tab paths in non-NUL short/porcelain output,
  status `-z`, status `--null`, status
  `--no-null`, status `--branch`, status `--no-branch`, status
  `--no-short`, `--no-porcelain`, no-value display option errors,
  unsupported porcelain-version errors, and `--long` / `-z` conflict errors,
  combined status `-sb` / `-bs`, status `-u` /
  `--untracked-files`, status `--untracked-files=no`, status
  `--ignored[=traditional|matching|no]` / `--no-ignored` no-op behavior in
  non-ignored fixtures, status rename toggles
  `--no-renames` / `--renames` / `-M[<n>]` / `--find-renames[=<n>]`,
  including rename-toggle value errors, status `--branch` upstream
  ahead/behind headers for covered same/ahead/behind/divergent/gone
  local-remote histories including `--no-ahead-behind`, matching long-status
  tracking summaries for the same histories, and simple status
  display toggles for verbosity and column output including
  `--column=auto` / `--column=never` / `--column=plain`, plus status
  `--show-stash` / `--no-show-stash` long-output stash count display,
  `--ignore-submodules[=none|untracked|dirty|all]` no-op behavior in
  non-submodule fixtures, covered invalid status mode/value diagnostics for
  untracked, ignored, ignore-submodules, and column options, simple nested-cwd
  status path display, and simple
  literal status pathspec filtering for files, directories, missing paths, NUL
  output, `--` pathspec separation, and detached-HEAD long/branch status
  headers,
  Rust-created branch interop, `branch -r`/`-a`, `branch --list <pattern>...`,
  remote/all branch list patterns, `branch --points-at` including local/remote/all list-pattern forms, `branch --contains`,
  `branch --no-contains`, including default-`HEAD` contains/no-contains forms,
  single-filter local/remote/all list-pattern forms, and combined positive/negative filters including equals-spelled and local/remote/all list-pattern forms,
  `branch --merged`, `branch --no-merged` including single-filter local/remote/all list-pattern forms and combined positive/negative filters including equals-spelled and local/remote/all list-pattern forms,
  branch listing `--no-color` / `--color` / `--color=always` /
  `--color=never` / `--color=auto`, covered order-sensitive local color
  toggles plus remote/all color reset ordering and remote/all display no-op
  plus local/remote/all `--list` no-op color combinations, `--no-column` /
  `--column=auto` / `--column=never` / `--column=plain` including remote/all
  display no-op, paired column no-op ordering, and local/remote/all `--list` combinations, and branch listing `--abbrev` /
  `--abbrev=<n>` / `--no-abbrev` no-op behavior for non-verbose local/remote/all output
  including paired reset/no-op ordering and local/remote/all `--list` combinations,
  branch listing `--no-delete` / `--no-list` / `--no-show-current` display no-op list combinations,
  `branch --no-points-at` no-op local/remote/all list combinations,
  plus branch listing `--sort=refname` / `--sort refname` / `--sort=-refname` / `--sort -refname`,
  version-aware `--sort=version:refname` / `--sort=v:refname` including descending forms,
  `--sort=objectname` / `--sort objectname`, `--sort=objecttype` / `--sort objecttype`,
  `--sort=objectsize` / `--sort objectsize`, date keys `--sort=authordate` /
  `--sort=committerdate` / `--sort=creatordate` including spaced and descending forms,
  `--sort=upstream` / `--sort upstream`, and `--sort=push` / `--sort push`
  including descending forms, and `--no-sort`
  for local/remote/all output including remote/all `--list` combinations
  and order-sensitive ascending/descending/version-aware/objectname/objecttype/objectsize/date/upstream/push sort/no-sort reset combinations,
  branch listing `--omit-empty` / `--no-omit-empty` display no-op parsing
  for local/remote/all output and `--list` combinations, including paired reset ordering,
  and local/remote/all branch `--list` pattern matching plus no-pattern display
  forms with `--ignore-case` / `-i` and `--no-ignore-case` reset ordering, and basic local/remote/all
  colored local/remote/all list-pattern combinations, and basic
  branch `--format` / `--format=<format>` output, including both spelling forms with list-pattern
  combinations and covered `--ignore-case` / `--no-ignore-case` pattern ordering,
  covered empty-format `--omit-empty` / `--no-omit-empty` combinations and list-first ordering for both spelling forms,
  and order-sensitive local/remote/all `--no-format` reset/no-op combinations with list-pattern coverage, using refname, objectname, HEAD,
  objecttype, objectsize, objectsize:disk, upstream tracking, and push tracking atoms,
  current-branch reporting, branch upstream set/unset config interop for local and configured remote upstreams,
  branch creation tracking config interop for direct/no-track/inherit modes,
  branch rename/copy action interop with reflog/config updates and current-branch HEAD movement,
  verbose branch-list interop with upstream tracking annotations,
  loose branch deletion observed with upstream
  `git branch --list`/`--show-current`, and remote-tracking branch deletion
  observed with upstream `git branch -r`,
  Rust checkout interop with upstream `git branch --show-current`,
  `git rev-parse`, and `git status --short`,
  Rust-created lightweight tag interop with upstream `git tag --list` and
  `git show-ref --tags`, CLI parity for `git tag -l <pattern>`,
  `git tag --list <pattern>...`, `git tag --points-at` / `--no-points-at`,
  `git tag --contains` / `--no-contains` with explicit or default `HEAD`,
  `git tag --merged`, and `git tag --no-merged` over
  lightweight and annotated tags, `git tag --` option-terminator handling for
  covered create, delete, list, and verify operands, upstream-style tag target
  resolution and too-many-argument errors for covered create paths,
  upstream-style malformed-object errors for covered tag filter revisions and
  empty/value-bearing tag filter spellings, option-looking `--merged` /
  `--no-merged` operands, and covered malformed/unknown tag sort keys, tag listing `--color` / `--no-color` /
  `--color=always` / `--color=auto` / `--color=never` / `--no-column` /
  `--column` / `--column=always` / `--column=auto` / `--column=never` /
  `--column=plain` / `--column=column` / `--column=row` /
  `--column=dense` / `--column=nodense` and covered comma-separated
  `--column` style combinations for covered non-wrapping output,
  upstream-style value errors for covered tag listing options and bare
  `--points-at` default-`HEAD` behavior,
  tag listing `--omit-empty` / `--no-omit-empty`,
  tag listing `--ignore-case` / `-i` / `--no-ignore-case` for patterns and text sort keys, and
  tag listing `--sort` / `--no-sort` for refname, version-aware refname, objectname, objecttype, objectsize, loose objectsize:disk, direct/peeled deltabase/raw-size/object metadata, direct/peeled date, direct/peeled identity, annotated tag header, direct/peeled commit topology, and direct/peeled contents subject/body/size keys,
  and basic tag listing `--format` / `--no-format` using ref, refname
  strip/rstrip/lstrip, object abbreviation, color, peeled-object, tagger,
  date, and contents atoms including line-limited contents, plus tag listing
  `-n` / `-n<num>` message annotations including covered `k/m/g` suffix
  parsing and invalid/range errors,
  tag `--create-reflog` / `--no-create-reflog` creation parsing and reflog
  entries for lightweight, annotated, and forced tag updates,
  annotated tag message cleanup modes `--cleanup=strip`, `--cleanup=whitespace`,
  `--cleanup=verbatim`, and `--no-cleanup`, tag `--no-edit` no-op parsing for
  covered non-editor creation paths, tag `--edit` / `-e` creation parsing for
  covered message-provided and no-message editor-noop paths, annotated tag
  no-message fatal handling and trailer-only message creation, empty
  `--file=` no-op handling,
  `--no-local-user` no-op parsing plus
  missing-value errors for `-m` / `--message`, `-F` / `--file`, and
  `-u` / `--local-user`,
  takes-no-value errors for covered boolean tag option `=<value>` forms,
  unknown-option errors for covered non-existent tag mode negations and
  `=<value>` forms, unknown-switch errors for covered short boolean
  `=<value>` forms, generic unknown long-option, covered unknown
  short-switch errors, and covered compact short-switch usage errors,
  annotated tag `--trailer` insertion for covered token/value forms and
  `--no-trailer` reset behavior,
  tag `-v` / `--verify` unsigned annotated, lightweight, missing, and
  multi-tag failure output,
  Rust-created loose tag deletion observed by upstream `git tag --list`,
  Rust-created annotated tag objects readable by upstream `git cat-file`,
  `diff --name-status`, `diff --name-only`, HEAD diff comparison, diff
  `--exit-code` / `--quiet`, diff `-z`, diff `--diff-filter`, and cached/staged diff parity
  for simple add/modify/delete cases, path-limited diff from repository root
  and nested current directories, exact rename name-status/name-only output
  including pathspec-limited rename splitting, exact copy name-status/name-only
  output including harder-copy source discovery, testkit diff interop coverage
  for rename/copy name output, diff `--summary` output for create/delete,
  mode-change, and exact rename/copy entries, plus diff name output quoting for
  paths with spaces, embedded quotes, and tabs, and diff `--raw` output for
  add/delete/modify, exact rename/copy entries, explicit raw object ID
  abbreviation widths, full IDs, and `core.abbrev` defaults, plus diff
  `--stat`, `--compact-summary`, `--numstat`, and `--shortstat` output for
  add/delete/modify, binary files, mode changes, and exact rename/copy entries,
  default text patch output for simple add/delete/modify worktree, HEAD, and
  cached comparisons including prefix, index-line abbreviation, and one-line
  hunk range controls plus cached mode-only and mode-and-content patch output,
  explicit `-p` / `-u` / `--patch`, `--patch-with-raw`, and
  `--patch-with-stat` aliases, order-sensitive `-s` / `--no-patch`
  suppression and output-mode resets, `--name-only` /
  `--name-status` precedence over raw/stat/numstat modes, combined
  `--summary` output with raw/stat/numstat/shortstat modes, and combinations
  with raw, stat, numstat, shortstat, and summary output,
  binary patch summaries for add/delete/modify and mode-only cases,
  exact rename/copy patch similarity headers including mode-changing renames,
  patch path quoting for spaces, quotes, and tabs,
  diff `--default-prefix` plus separate-value `--src-prefix` / `--dst-prefix`,
  `rev-parse` parity for HEAD, branches, tags, full object IDs, parent suffixes
  over an upstream-created merge history, and peel suffixes over an annotated
  tag, plus `rev-parse --abbrev-ref`, `--symbolic-full-name`, `--verify`,
  `--verify --quiet` missing-revision status/output, `--verify` separator
  handling, `--short`, `--show-toplevel`, `--show-prefix`, `--show-cdup`
  path output and outside-worktree status behavior, `--path-format` absolute
  and relative path output plus invalid path-format errors,
  `.git` file discovery for covered submodule worktrees,
  `--show-superproject-working-tree`, `--show-object-format` in SHA-1 and SHA-256 repositories
  plus invalid-mode errors,
  `--show-ref-format`, `--local-env-vars`,
  `--git-dir`, `--absolute-git-dir`, `--git-common-dir`, `--git-path`
  including covered `GIT_OBJECT_DIRECTORY` and `GIT_INDEX_FILE` overrides,
  `--resolve-git-dir`, `--is-inside-work-tree`,
  `--is-inside-git-dir`, `--is-bare-repository` across worktree, git-dir,
  and bare-repository directories, and
  `--is-shallow-repository`,
  `ls-tree`, `ls-tree -z`, `ls-tree --name-only`, `ls-tree --name-status`,
  `ls-tree --object-only`, `ls-tree --long`, recursive `ls-tree -r`, tree entry inclusion with
  `ls-tree -t` / `ls-tree -r -t`, directories-only output with
  `ls-tree -d` / `ls-tree -r -d`, object id abbreviation with
  `ls-tree --abbrev[=<n>]` / `--no-abbrev`, nested current-directory output,
  `ls-tree --full-name`, `ls-tree --full-tree`, minimal `ls-tree --format`
  placeholders, path quoting for space, quote, and tab paths in non-NUL output,
  `--no-full-name` / `--no-full-tree` negation handling, and literal `ls-tree <path>` filtering plus `--` pathspec separation for tree IDs,
  commits, and annotated tags, `cat-file` for revision/ref names,
  `cat-file --batch`, `cat-file --batch-check`, minimal
  `cat-file --batch=<format>` / `cat-file --batch-check=<format>` /
  `cat-file --batch-command=<format>` placeholders including disk size and delta base, `cat-file -z` / `-Z`,
  `cat-file --batch-all-objects`, minimal `cat-file --batch-command`, mailmap no-op flags,
  and batch tuning no-op flags,
  `log` formatting plus `log --oneline` / `log --pretty=oneline` /
  `log --format=oneline` plus minimal custom `log --format` placeholders for
  commit, tree, parent, subject/body, sanitized subject, hex escapes, decoration, encoding, no-notes notes and covered no-op notes controls/aliases, covered no-op color controls, no-reflog reflog fields, single-source source names, unsigned-signature, revision marker, author, committer, timestamp, fixed date, covered `--date`-selected default date, and non-mailmap identity fields
  over commits and annotated tags, `log --reverse`, log quiet/source/mailmap/
  decoration auto/empty/false, clearing/filtering/disabling no-op flags for covered no-mailmap cases,
  encoding output selection no-op behavior for covered UTF-8 commits,
  and short/full `--decorate` labels for covered oneline/pretty-oneline modes,
  walk-mode (`--no-walk` and `--first-parent` for covered single-revision cases), default date-ready walk ordering, explicit ordering (`--topo-order`, `--date-order`, and `--author-date-order`) for covered single-revision walks, no-op history simplification flags (`--sparse`, `--dense`, `--remove-empty`, `--unpacked`, `--full-history`, `--simplify-merges`, and `--show-pulls`) for covered path-free walks, ref selectors (`--all`, `--branches[=<glob>]`, `--tags[=<glob>]`, `--remotes[=<glob>]`, and `--glob[=<glob>]` with scoped `--exclude` plus `--exclude-hidden[=<section>]` configured hidden-ref filtering for covered `--all` / `--glob` modes) for covered local ref walks, `--default <rev>` fallback when no explicit start revision is supplied, `--stdin` revision input for covered LF-delimited revision and `--not` toggle cases, multiple positive revisions, `^rev` / `--not` exclusions, and simple `A..B` / `A...B` ranges for covered log walks, identity filters (`--author[=<pattern>]` and `--committer[=<pattern>]`), message filters (`--grep[=<pattern>]`, `--all-match`, and `--invert-grep`), filter case folding (`-i` / `--regexp-ignore-case`), regexp mode flags (`-F` / `--fixed-strings`, `-E`, `--basic-regexp`, and `--extended-regexp`) for covered literal and simple regex cases, and timestamp filters (`--max-age[=<epoch>]`, `--min-age[=<epoch>]`, plus `--since` / `--after` / `--until` / `--before` for covered `@<epoch> <tz>` and explicit `YYYY-MM-DD[ T]HH:MM:SS <tz>` forms), parent-count filters (`--merges`, `--no-merges`, and min/max parent forms), abbreviation control (`--abbrev-commit`, `--no-abbrev-commit`, `--abbrev[=<n>]`, and `--no-abbrev`) for covered commit-header/oneline/custom placeholder modes, `--parents` commit-header output for covered default/oneline modes, `--children` oneline header output, plus date-display/notes-signature-disabling/unsigned-signature-display/color-mode/no-op color-placeholder/patch-suppression/merge-diff-disabling/diff-display/rename/copy-detection/rewrite-detection/diff-algorithm/anchored-diff/whitespace-diff/context-control/prefix-control/index-abbrev/submodule/textconv/color-moved/ws-error-highlight/pickaxe-mode no-op flags for covered no-patch cases, single-object SHA-1/SHA-256
  pack-read, SHA-1/SHA-256 deltified pack-read, SHA-1 thin pack-read with
  external base lookup, and single-object
  SHA-1/SHA-256 pack-index parity cases, ODB reads from SHA-1/SHA-256
  upstream-written packs including deltified packs, upstream verification of
  Rust-written SHA-1/SHA-256 undeltified and deltified pack/index and SHA-256
  loose object data,
  loose-ref interop with upstream `git show-ref`, minimal `pack-refs`
  support for covered default tag packing, `--all`, `--prune`,
  `--no-prune`, `--no-all`, `--include`, `--exclude`, `--no-include`,
  `--no-exclude`, unknown-option/no-value usage, loose-ref pruning, and
  upstream-style peeled annotated-tag packed-ref output, and filtered
  `show-ref --head`, `show-ref --heads/--branches/--tags` plus filter
  negations, `--hash` / `-s` / `-s<n>` including compact `-d`/`-q`/`-s` clusters /
  `--no-hash`, `--abbrev` / `--no-abbrev`,
  `--dereference` / `--no-dereference`, suffix-component pattern filters, `--`
  ref separation, `show-ref --exclude-existing[=<pattern>]`, and unmatched
  filter exit status, verified `show-ref --verify HEAD` / `--verify --quiet`
  / `--no-quiet` output and missing-ref exit/status behavior, `show-ref
  --exists` / `--no-exists` including `HEAD` and arity errors
  status/output, Rust-written packed-ref files and compacted/pruned packed refs
  including peeled annotated tags read by upstream `git show-ref`, and symbolic
  `HEAD` read/write, timestamp-cutoff reflog expiration observed by upstream
  `git reflog show`, `git reflog list` output/error parity for covered reflog
  sets, local-path and configured-local-remote `ls-remote` default, filter,
  symref, pattern, refname/version/objectname/objecttype/objectsize sort,
  objectname/objecttype/objectsize outside-repository fatal behavior,
  objecttype/objectsize missing-object failures, bad sort key, `--exit-code`, and
  `--get-url` parity, plus quiet local-repository `clone <path> [<directory>]`
  using upstream-style default destination naming for repository `.git` paths,
  configured `origin` fetch refspecs, `--bare` clone with direct branch refs,
  bare default destination naming and bare `--branch` / `--no-tags` config/ref
  behavior, `--mirror` clone with all-refs refspec, mirror config, tagOpt, and
  `--no-mirror` restoration of non-bare clone behavior, default-branch checkout and
  `--origin <name>` / `--origin=<name>` / `-o <name>` / `-o<name>` custom
  remote names across normal, bare, and mirror local clones, plus
  `--no-origin` reset behavior for covered normal local clones,
  `-v` / `--verbose` / `--no-verbose` output and ref/config parity for
  covered local clone cases,
  `--progress` / `--no-progress` output and ref/config parity for covered
  local clone cases,
  accepted local-clone transport toggles `-l` / `--local` / `--no-local`
  and `--hardlinks` / `--no-hardlinks` for covered local clone outputs,
  `-u <path>` / `-u<path>` / `--upload-pack <path>` /
  `--upload-pack=<path>` / `--no-upload-pack` accepted local-clone upload-pack
  selector forms for covered local clone outputs,
  `--server-option <value>` / `--server-option=<value>` /
  `--no-server-option` accepted local-clone server-option forms for covered
  local clone outputs,
  `-j <n>` / `-j<n>` / `--jobs <n>` / `--jobs=<n>` / `--no-jobs`
  accepted local-clone submodule job-count forms for covered no-submodule local
  clone outputs,
  IP-family hints `-4` / `--ipv4` / `-6` / `--ipv6` for covered local
  clone outputs,
  `--reject-shallow` / `--no-reject-shallow` parity for covered non-shallow
  local clones,
  local-clone ignored shallow hints `--depth <n>` / `--depth=<n>` /
  `--shallow-since <time>` / `--shallow-since=<time>` /
  `--shallow-exclude <rev>` / `--shallow-exclude=<rev>` plus reset forms
  for covered normal, bare, and mirror local clone outputs,
  local-clone ignored partial clone filters `--filter <spec>` /
  `--filter=<spec>` plus `--no-filter` reset behavior, warning output, and
  `remote.<name>.promisor` / `remote.<name>.partialclonefilter` config parity
  for covered normal, bare, and mirror local clone outputs,
  `--bundle-uri <uri>` / `--bundle-uri=<uri>` local-file bundle prefetch,
  `refs/bundles/*` imported bundle refs, missing-bundle warning continuation,
  `--no-bundle-uri` reset behavior, and shallow option conflict diagnostics for
  covered normal, bare, and mirror local clone outputs,
  `--sparse` normal local clone initialization with sparse checkout config,
  root-only worktree materialization, skip-worktree index entries, and
  `--no-sparse` reset behavior for covered local clone outputs,
  `--separate-git-dir <gitdir>` / `--separate-git-dir=<gitdir>`
  gitfile and external git-dir layout parity, `--no-separate-git-dir` reset
  behavior, and bare/mirror conflict diagnostics for covered local clone
  outputs,
  `--recurse-submodules[=<pathspec>]` / `--recursive[=<pathspec>]`
  `submodule.active` config parity plus reset forms for covered no-submodule
  local clone outputs,
  `--remote-submodules` / `--no-remote-submodules`,
  `--shallow-submodules` / `--no-shallow-submodules`, and
  `--also-filter-submodules` / `--no-also-filter-submodules` final-state
  acceptance/reset and missing `--filter` / missing `--recurse-submodules`
  diagnostics for covered no-submodule local clone outputs,
  accepted negative no-op clone flags `--no-recurse-submodules` /
  `--no-recursive`, `--no-sparse`, `--no-filter`,
  `--no-also-filter-submodules`, `--no-remote-submodules`,
  `--no-shallow-submodules`, and `--no-bundle-uri` for covered local clone
  outputs, plus additional standalone negative no-op clone flags
  `--no-depth`, `--no-shallow-since`, `--no-shallow-exclude`,
  `--no-shared`, `--no-reference`, `--no-reference-if-able`,
  `--no-dissociate`, `--no-separate-git-dir`, `--no-template`,
  `--no-jobs`, and `--no-revision` for covered local clone outputs,
  `--ref-format=files` / `--ref-format files` / `--no-ref-format`
  file-ref clone output parity across normal, bare, and mirror local clones,
  `--template <dir>` / `--template=<dir>` custom template file copying and
  template config merge parity across normal, bare, and mirror local clones,
  plus missing-template warning parity for covered quiet local clones,
  `-s` / `--shared`, `--reference <repo>` / `--reference=<repo>`,
  `--reference-if-able <repo>` / `--reference-if-able=<repo>`,
  reference reset forms, `--dissociate` / `--no-dissociate`, and
  `objects/info/alternates` file parity for covered local clone outputs,
  `-c <key=value>` / `--config <key=value>` / `--config=<key=value>`
  persisted clone-time config across normal, bare, and mirror local clones,
  `--branch <branch>` / `--branch=<branch>` / `-b<branch>` checkout and
  `--no-branch` reset behavior, `--revision <rev>` / `--revision=<rev>`
  detached-HEAD local clone output, absent fetch refspec/ref creation,
  `--no-revision` reset behavior, bare revision clone output, and
  branch/mirror conflict diagnostics, `--single-branch` / `--no-single-branch`
  refspec, ref, and reachable-tag auto-follow behavior for normal, bare, and
  mirror local clones, `--no-checkout`, remote-tracking refs, tags,
  `--no-tags` tag suppression with `remote.<name>.tagOpt` config, `--tags`
  reset behavior, `<name>/HEAD`, and branch upstream config, plus
  `symbolic-ref --quiet` / `--no-quiet`, `--short` / `--no-short`,
  `--recurse` / `--no-recurse`, `--delete` / `--no-delete`, `--` ref
  separation, and update `-m` / `-m<reason>` option interop with upstream
  `git symbolic-ref`.
- Reftable, broader commit-graph acceleration beyond parent suffixes and split
  graph writing, broader MIDX generation and maintenance integration, bitmaps, broader fetch/clone/push workflows, protocols, broader submodule workflows,
  hooks, filters, and maintenance/gc are not implemented.
