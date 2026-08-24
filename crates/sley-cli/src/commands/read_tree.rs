//! `git read-tree` — read tree object(s) into the index.
//!
//! This is a self-contained reimplementation of the `read-tree` plumbing
//! command. It reads one to three tree-ish objects into the on-disk index,
//! optionally merging them (`-m`), reading them under a path prefix
//! (`--prefix=<p>`), discarding unmerged/local index state (`--reset`), and/or
//! updating the working tree to match (`-u`). With no merge mode it overlays the
//! given trees into stage 0 (later trees win on path collisions); `--empty`
//! clears the index entirely.
//!
//! The behaviours mirror upstream `git read-tree`:
//!
//! * **Read (no `-m`/`--reset`/`--prefix`)** — replace the index with the union
//!   of the listed trees in stage 0. The working tree is never touched and no
//!   "up to date" checks run. `-u` is rejected in this mode.
//! * **`--reset`** — like a one-tree read that also drops any higher-stage
//!   (conflicted) entries without complaint; with `-u` the working tree is
//!   reset to the tree as well.
//! * **`--prefix=<p>`** — overlay a single tree under `<p>/` into the current
//!   index (stage 0), keeping every other entry.
//! * **`-m`** — perform the trivial three-way (or two-way / fast-forward)
//!   merge, emitting stage 1/2/3 entries for paths git cannot resolve trivially.
//!
//! All shared CLI helpers (repository discovery, object-format detection,
//! revision resolution, the engine crates) come from the crate root via the
//! `use crate::*;` glob, matching the `commands::stash` / `branch` / `tag`
//! pattern.
#![allow(clippy::expect_used)]

use crate::*;
use sley::plumbing::{sley_diff_merge, sley_index, sley_rev, sley_worktree};
// Engine plumbing moved down into `sley-worktree` (stage A): the unpack-trees
// worktree probe/writer and its helpers now live in the published engine; the
// porcelain keeps thin call wrappers with these aliases.
use sley_worktree::{
    ReadTreeWorktree, SubmoduleHooks, gitlink_should_recurse, load_superproject_submodules,
    prune_empty_dirs, refuse_if_unpack_entries_turn_cwd_into_file,
    refuse_if_unpack_result_removes_current_directory, remove_path_in_the_way,
    remove_worktree_path, safe_worktree_path, write_tree_entry_to_worktree,
};
pub(crate) use sley_worktree::{UnpackPorcelain, checkout_two_way_engine};

/// Which top-level operation `read-tree` should perform. These mirror git's
/// `--reset` / `--prefix` / `-m` switches, which are mutually exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadTreeMode {
    /// Plain read: overlay the listed trees into stage 0 (no worktree update,
    /// no up-to-date checks). This is the default when none of the merge
    /// switches are given.
    Read,
    /// `--reset`: like a one-tree read that also discards higher-stage entries.
    Reset,
    /// `--prefix=<p>`: overlay one tree under `<p>/` into the current index.
    Prefix(Vec<u8>),
    /// `-m`: trivial fast-forward / two-way / three-way merge.
    Merge,
}

/// Parsed command-line options for `read-tree`.
#[derive(Debug)]
struct ReadTreeArgs {
    mode: ReadTreeMode,
    update_worktree: bool,
    recurse_submodules: Option<bool>,
    sparse_checkout: bool,
    empty: bool,
    /// git's `-n` / `--dry-run`: compute (and validate) the merge without
    /// writing the index or touching the worktree. Used by the upstream test
    /// harness's `read_tree_*_must_succeed` to prove the dry run leaves index
    /// and worktree untouched before the real run.
    dry_run: bool,
    /// git's `-i`: don't check that the working tree is up to date and don't
    /// require one at all (`opts.index_only`). This is what lets `read-tree -i
    /// -m` run inside a bare repository against `$GIT_INDEX_FILE`.
    index_only: bool,
    trees: Vec<String>,
}

type StagedEntry = sley_worktree::ReadTreeEntry;

pub(crate) fn cmd_read_tree(cli_session: &session::CliSession, args: &[String]) -> Result<()> {
    let parsed = parse_read_tree_args(args)?;

    let repo = RepositoryContext::from_session(cli_session)?;
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();
    let recurse_submodules = parsed.recurse_submodules.unwrap_or_else(|| {
        repo.config()
            .get_bool("submodule", None, "recurse")
            .unwrap_or(false)
    });

    // `--empty` (and the deprecated no-argument spelling) just clears the index.
    if parsed.empty {
        sley_worktree::persist_read_tree_entries(git_dir, format, Vec::new())?;
        return Ok(());
    }

    // Resolve every positional argument to a tree object id up front so an
    // invalid tree-ish aborts before any index mutation (matching git).
    let mut tree_oids = Vec::with_capacity(parsed.trees.len());
    for tree in &parsed.trees {
        tree_oids.push(resolve_tree_ish(&repo, tree)?);
    }
    if read_tree_check_cache_tree() {
        for tree_oid in &tree_oids {
            if sley_diff_merge::tree_has_duplicate_leaf_paths(db, format, tree_oid)? {
                return Err(sley_diff_merge::corrupted_cache_tree_error());
            }
        }
    }

    // git's `-n` / `--dry-run`: run the merge to validate it (and surface the
    // same errors / exit code), but leave the index and worktree untouched. The
    // upstream `read_tree_*_must_succeed` helper relies on this to prove the dry
    // run is a no-op before the real run. `update` is forced off so no file is
    // written, and the resulting index is discarded rather than persisted.
    let apply_worktree = parsed.update_worktree && !parsed.dry_run;

    match &parsed.mode {
        ReadTreeMode::Read => {
            let entries = plan_read_tree_entries(
                &repo,
                &tree_oids,
                sley_worktree::ReadTreeTransitionMode::Overlay,
            )?;
            if !parsed.dry_run {
                // Git's cache-tree update prefetches every missing non-gitlink
                // index blob in one promisor request before verifying existence
                // (t1022). Mirror that batch boundary when the repo is partial.
                prefetch_read_tree_index_blobs(git_dir, db, &entries, cli_session.lazy_fetch())?;
                sley_worktree::persist_read_tree_entries(git_dir, format, entries)?;
                if parsed.update_worktree && parsed.sparse_checkout {
                    apply_read_tree_sparse_checkout(
                        repo.worktree_root()?,
                        git_dir,
                        format,
                        repo.config(),
                    )?;
                }
            }
        }
        ReadTreeMode::Reset => {
            // `--reset` accepts up to three trees but only the resulting union
            // matters; higher-stage entries are simply dropped (we never create
            // them here). With `-u` the worktree is updated to match.
            let mut entries = plan_read_tree_entries(
                &repo,
                &tree_oids,
                sley_worktree::ReadTreeTransitionMode::Overlay,
            )?;
            if apply_worktree {
                let worktree_root = repo.worktree_root()?;
                let reset_result = reset_worktree_to_entries(
                    worktree_root,
                    git_dir,
                    format,
                    db,
                    repo.config(),
                    None,
                    &mut entries,
                    recurse_submodules,
                );
                if reset_result.is_err() {
                    // A `-u --reset --recurse-submodules` that fails on a missing
                    // submodule commit has already rewritten the superproject
                    // worktree with fresh mtimes but never persisted the new index
                    // stat; git leaves the index refreshed so `git diff-files`
                    // stays clean. Refresh the on-disk index against the worktree
                    // before propagating so a rewritten superproject path is not
                    // reported as a phantom modification (`ie_match_stat` compares
                    // size+mtime, not content).
                    let _ = sley_worktree::refresh_index_paths(
                        worktree_root,
                        git_dir,
                        format,
                        &[],
                        /* quiet */ true,
                        /* ignore_missing */ true,
                        /* really_refresh */ false,
                    );
                }
                reset_result?;
            }
            if !parsed.dry_run {
                sley_worktree::persist_read_tree_entries(git_dir, format, entries)?;
                if parsed.update_worktree && parsed.sparse_checkout {
                    apply_read_tree_sparse_checkout(
                        repo.worktree_root()?,
                        git_dir,
                        format,
                        repo.config(),
                    )?;
                }
            }
        }
        ReadTreeMode::Prefix(prefix) => {
            let mut entries = plan_read_tree_entries(
                &repo,
                &tree_oids,
                sley_worktree::ReadTreeTransitionMode::Prefix(prefix.clone()),
            )?;
            if apply_worktree {
                let worktree_root = repo.worktree_root()?;
                update_worktree_for_entries(
                    worktree_root,
                    git_dir,
                    format,
                    db,
                    repo.config(),
                    None,
                    &mut entries,
                    recurse_submodules,
                )?;
            }
            if !parsed.dry_run {
                sley_worktree::persist_read_tree_entries(git_dir, format, entries)?;
                if parsed.update_worktree && parsed.sparse_checkout {
                    apply_read_tree_sparse_checkout(
                        repo.worktree_root()?,
                        git_dir,
                        format,
                        repo.config(),
                    )?;
                }
            }
        }
        ReadTreeMode::Merge => {
            // The trivial fast-forward / two-way / three-way merge now runs
            // through the shared `sley-unpack-trees` engine (git's
            // oneway/twoway/threeway_merge). The engine computes the result
            // index and the worktree update plan; we apply the plan with `-u`.
            let entries = merge_trees(
                if parsed.index_only {
                    repo.worktree_root().ok()
                } else {
                    Some(repo.worktree_root()?)
                },
                git_dir,
                format,
                db,
                repo.config(),
                &tree_oids,
                apply_worktree,
                parsed.index_only,
                parsed.sparse_checkout,
                recurse_submodules,
            )?;
            if !parsed.dry_run {
                sley_worktree::persist_read_tree_entries(git_dir, format, entries)?;
                if parsed.update_worktree && parsed.sparse_checkout {
                    apply_read_tree_sparse_checkout(
                        repo.worktree_root()?,
                        git_dir,
                        format,
                        repo.config(),
                    )?;
                }
            }
        }
    }

    Ok(())
}

struct CliReadTreeDiagnostics;

impl sley_worktree::ReadTreeDiagnostics for CliReadTreeDiagnostics {
    fn invalid_path(&mut self, path: &[u8]) {
        eprintln!("error: invalid path '{}'", String::from_utf8_lossy(path));
    }
}

fn plan_read_tree_entries(
    repo: &RepositoryContext,
    tree_oids: &[ObjectId],
    mode: sley_worktree::ReadTreeTransitionMode,
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    let mut diagnostics = CliReadTreeDiagnostics;
    Ok(
        map_read_tree_transition_result(sley_worktree::plan_read_tree_transition(
            repo.git_dir(),
            repo.format(),
            repo.objects(),
            repo.config(),
            sley_worktree::ReadTreeTransitionOptions { mode, tree_oids },
            &mut diagnostics,
        ))?
        .entries,
    )
}

fn map_read_tree_transition_result<T>(
    result: sley_worktree::ReadTreeTransitionResult<T>,
) -> Result<T> {
    match result {
        Ok(outcome) => Ok(outcome),
        Err(sley_worktree::ReadTreeTransitionError::InvalidPath(_)) => Err(GitError::Exit(128)),
        Err(sley_worktree::ReadTreeTransitionError::BindOverlap { incoming, existing }) => {
            eprintln!(
                "error: Entry '{}' overlaps with '{}'.  Cannot bind.",
                String::from_utf8_lossy(&incoming),
                String::from_utf8_lossy(&existing)
            );
            Err(GitError::Exit(128))
        }
        Err(sley_worktree::ReadTreeTransitionError::Engine(error)) => Err(error),
    }
}

fn apply_read_tree_sparse_checkout(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<()> {
    let worktree_config = GitConfig::read(git_dir.join("config.worktree")).unwrap_or_default();
    let repo_config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    let sparse_enabled = config
        .get_bool("core", None, "sparseCheckout")
        .or_else(|| worktree_config.get_bool("core", None, "sparseCheckout"))
        .or_else(|| repo_config.get_bool("core", None, "sparseCheckout"))
        .unwrap_or(false);
    if !sparse_enabled {
        return Ok(());
    }
    let sparse_file = git_dir.join("info").join("sparse-checkout");
    if !sparse_file.exists() {
        return Ok(());
    }
    let cone = config
        .get_bool("core", None, "sparseCheckoutCone")
        .or_else(|| worktree_config.get_bool("core", None, "sparseCheckoutCone"))
        .or_else(|| repo_config.get_bool("core", None, "sparseCheckoutCone"))
        .unwrap_or(false);
    let sparse_index = cone
        && config
            .get_bool("index", None, "sparse")
            .or_else(|| worktree_config.get_bool("index", None, "sparse"))
            .or_else(|| repo_config.get_bool("index", None, "sparse"))
            .unwrap_or(false);
    let bytes = fs::read(sparse_file)?;
    let mut patterns: Vec<Vec<u8>> = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if patterns.last().map(Vec::is_empty) == Some(true) {
        patterns.pop();
    }
    let mode = if cone && crate::commands::sparse_checkout::cone_patterns_are_valid(&patterns, true)
    {
        sley_worktree::SparseCheckoutMode::Cone
    } else {
        sley_worktree::SparseCheckoutMode::Full
    };
    let sparse = sley_worktree::SparseCheckout {
        patterns,
        sparse_index,
    };
    sley_worktree::apply_sparse_checkout_with_mode(worktree_root, git_dir, format, &sparse, mode)?;
    Ok(())
}

/// Parse `read-tree`'s arguments, enforcing the same mutual-exclusion and
/// arity rules as upstream git (and the matching `fatal:`/exit codes).
fn parse_read_tree_args(args: &[String]) -> Result<ReadTreeArgs> {
    let mut mode: Option<ReadTreeMode> = None;
    let mut update_worktree = false;
    let mut recurse_submodules = None;
    let mut sparse_checkout = true;
    let mut empty = false;
    let mut dry_run = false;
    let mut index_only = false;
    let mut trees = Vec::new();
    let mut no_more_options = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if no_more_options {
            trees.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => no_more_options = true,
            "-m" => set_mode(ReadTreeMode::Merge, &mut mode)?,
            "--reset" => set_mode(ReadTreeMode::Reset, &mut mode)?,
            "-u" => update_worktree = true,
            "-mu" | "-um" => {
                set_mode(ReadTreeMode::Merge, &mut mode)?;
                update_worktree = true;
            }
            "-i" => index_only = true, // "don't check the working tree" (git's index_only).
            "--empty" => empty = true,
            "--no-empty" => empty = false,
            "--recurse-submodules" => recurse_submodules = Some(true),
            "--no-recurse-submodules" => recurse_submodules = Some(false),
            "--prefix" => {
                let value = iter.next().ok_or_else(|| {
                    eprintln!("error: option `prefix' requires a value");
                    GitError::Exit(129)
                })?;
                set_mode(ReadTreeMode::Prefix(parse_prefix(value)?), &mut mode)?;
            }
            // Accepted no-op switches that don't change our deterministic output.
            "-v" | "--verbose" | "--no-verbose" | "-q" | "--quiet" | "--no-quiet" | "--trivial"
            | "--no-trivial" | "--aggressive" | "--no-aggressive" | "--debug-unpack"
            | "--no-debug-unpack" => {}
            "--no-sparse-checkout" => sparse_checkout = false,
            "--sparse-checkout" => sparse_checkout = true,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            value => {
                if let Some(prefix) = value.strip_prefix("--prefix=") {
                    set_mode(ReadTreeMode::Prefix(parse_prefix(prefix)?), &mut mode)?;
                } else if value.starts_with('-') && value != "-" {
                    // Unknown option: git's parse-options prints usage and exits
                    // 129. We surface the same exit code with a focused message.
                    eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                    return Err(GitError::Exit(129));
                } else {
                    trees.push(value.to_string());
                }
            }
        }
    }

    let mode = mode.unwrap_or(ReadTreeMode::Read);

    if empty {
        if !trees.is_empty() {
            eprintln!("fatal: passing trees as arguments contradicts --empty");
            return Err(GitError::Exit(128));
        }
        return Ok(ReadTreeArgs {
            mode,
            update_worktree,
            recurse_submodules,
            sparse_checkout,
            empty,
            dry_run,
            index_only,
            trees,
        });
    }

    validate_read_tree_arity(&mode, update_worktree, &trees)?;

    Ok(ReadTreeArgs {
        mode,
        update_worktree,
        recurse_submodules,
        sparse_checkout,
        empty,
        dry_run,
        index_only,
        trees,
    })
}

/// Select a merge-style `mode`, reporting git's collision message if one of the
/// mutually-exclusive `-m` / `--reset` / `--prefix` switches was already given.
fn set_mode(candidate: ReadTreeMode, current: &mut Option<ReadTreeMode>) -> Result<()> {
    if current.is_some() {
        eprintln!("fatal: Which one? -m, --reset, or --prefix?");
        return Err(GitError::Exit(128));
    }
    *current = Some(candidate);
    Ok(())
}

/// Enforce the per-mode argument-count rules and the `-u` placement rule.
fn validate_read_tree_arity(
    mode: &ReadTreeMode,
    update_worktree: bool,
    trees: &[String],
) -> Result<()> {
    // `-u` is only meaningful alongside a merge-style mode.
    if update_worktree && matches!(mode, ReadTreeMode::Read) {
        eprintln!("fatal: -u is meaningless without -m, --reset, or --prefix");
        return Err(GitError::Exit(128));
    }

    match mode {
        ReadTreeMode::Read => {
            // A plain read overlays any number of trees into stage 0, so there
            // is no upper bound; the only special case is the deprecated
            // empty-index spelling with no trees at all.
            if trees.is_empty() {
                eprintln!(
                    "warning: read-tree: emptying the index with no arguments is deprecated; use --empty"
                );
            }
        }
        ReadTreeMode::Reset | ReadTreeMode::Prefix(_) => {
            if trees.is_empty() {
                eprintln!("fatal: you must specify at least one tree to merge");
                return Err(GitError::Exit(128));
            }
        }
        ReadTreeMode::Merge => {
            if trees.is_empty() {
                eprintln!("fatal: you must specify at least one tree to merge");
                return Err(GitError::Exit(128));
            }
            if trees.len() > sley_unpack_trees::MAX_UNPACK_TREES {
                eprintln!(
                    "fatal: I cannot read more than {} trees",
                    sley_unpack_trees::MAX_UNPACK_TREES
                );
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(())
}

/// Normalize a `--prefix` value to git's canonical form: a non-empty path that
/// ends in a single `/` (an empty prefix becomes the root sentinel `/`).
fn parse_prefix(value: &str) -> Result<Vec<u8>> {
    if value.starts_with('/') {
        eprintln!("fatal: Invalid prefix, prefix cannot start with '/'");
        return Err(GitError::Exit(128));
    }
    Ok(normalize_prefix(value))
}

fn normalize_prefix(value: &str) -> Vec<u8> {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        return b"/".to_vec();
    }
    let mut out = trimmed.as_bytes().to_vec();
    out.push(b'/');
    out
}

/// The canonical empty-tree object id for `format`. git knows this tree
/// implicitly, so `read-tree <empty-tree>` succeeds even when the object was
/// never written to the store.
fn empty_tree_oid(format: ObjectFormat) -> Result<ObjectId> {
    EncodedObject::new(ObjectType::Tree, Vec::new()).object_id(format)
}

/// Resolve a tree-ish CLI argument to the object id of its tree, peeling
/// commits and tags. Reports git's `Not a valid object name` on failure.
fn resolve_tree_ish(repo: &RepositoryContext, spec: &str) -> Result<ObjectId> {
    let format = repo.format();
    let oid = match repo.resolve_revision(spec) {
        Ok(oid) => oid,
        Err(_) => {
            eprintln!("fatal: Not a valid object name {spec}");
            return Err(GitError::Exit(128));
        }
    };
    // The empty tree is valid even when it is not physically stored.
    if oid == empty_tree_oid(format)? {
        return Ok(oid);
    }
    match sley_rev::peel_to_tree(repo.objects(), format, &oid) {
        Ok(tree) => Ok(tree),
        Err(_) => {
            eprintln!("fatal: Not a valid object name {spec}");
            Err(GitError::Exit(128))
        }
    }
}

fn read_tree_check_cache_tree() -> bool {
    !matches!(
        std::env::var("GIT_TEST_CHECK_CACHE_TREE").as_deref(),
        Ok("false" | "0")
    )
}

/// Prefetch every missing non-gitlink blob referenced by the planned index,
/// matching git's `cache_tree_update` → `prefetch_cache_entries` path used when
/// a promisor remote is configured (t1022).
fn prefetch_read_tree_index_blobs(
    git_dir: &Path,
    db: &FileObjectDatabase,
    entries: &[(Vec<u8>, sley_worktree::ReadTreeEntry)],
    lazy_fetch: bool,
) -> Result<()> {
    if !lazy_fetch || entries.is_empty() {
        return Ok(());
    }
    // Only partial clones hydrate on cache-tree write; plain repos no-op.
    let config = match crate::commands::remote::read_repo_config(git_dir) {
        Ok(config) => config,
        Err(_) => return Ok(()),
    };
    if crate::promisor_remote_names(&config).is_empty() {
        return Ok(());
    }
    let mut oids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_path, entry) in entries {
        if sley_index::is_gitlink(entry.mode) {
            continue;
        }
        if seen.insert(entry.oid) {
            oids.push(entry.oid);
        }
    }
    crate::prefetch_promisor_objects(db, &oids, true)
}

/// Reset both index and worktree to `commit`, using the recursive gitlink writer
/// when requested. This is the reset/read-tree equivalent of git's
/// `read-tree -u --reset --recurse-submodules <commit>`.
pub(crate) fn reset_index_and_worktree_to_commit(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    commit: &ObjectId,
    recurse_submodules: bool,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let config = read_repo_config(git_dir).unwrap_or_default();
    let tree = commands::merge_rebase::commit_tree_oid(&db, format, commit)?;
    let mut diagnostics = CliReadTreeDiagnostics;
    let entries = map_read_tree_transition_result(sley_worktree::plan_read_tree_transition(
        git_dir,
        format,
        &db,
        &config,
        sley_worktree::ReadTreeTransitionOptions::overlay(&[tree]),
        &mut diagnostics,
    ))?
    .entries;
    let index_path = sley_worktree::repository_index_path(git_dir);
    let previous_index = fs::read(&index_path).ok();
    let target_paths = entries
        .iter()
        .map(|(path, _)| path.as_slice())
        .collect::<BTreeSet<_>>();
    if recurse_submodules
        && let Some(index) = sley_worktree::read_repository_index(git_dir, format)?
    {
        for entry in index.entries.iter().filter(|entry| {
            entry.stage() == sley_index::Stage::Normal
                && sley_index::is_gitlink(entry.mode)
                && !target_paths.contains(entry.path.as_bytes())
        }) {
            remove_submodule_worktree(worktree_root, git_dir, &entry.path)?;
        }
    }

    // The shared hard-reset engine owns sparse pattern application and
    // physical sparse-index shape preservation. The old read-tree overlay
    // materialized every target leaf and relied on a porcelain reapply pass,
    // which both expanded out-of-cone paths and collapsed partial sparse-index
    // boundaries. Reset the superproject first, then recurse only into
    // materialized target gitlinks.
    sley_worktree::reset_index_and_worktree_to_commit(worktree_root, git_dir, format, commit)?;
    if !recurse_submodules {
        return Ok(());
    }
    let persisted = sley_worktree::read_repository_index(git_dir, format)?
        .map(|index| {
            index
                .entries
                .into_iter()
                .filter(|entry| entry.stage() == sley_index::Stage::Normal)
                .map(|entry| (entry.path.as_bytes().to_vec(), entry))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    for (path, entry) in entries
        .iter()
        .filter(|(_, entry)| sley_index::is_gitlink(entry.mode))
    {
        if persisted
            .get(path)
            .is_some_and(sley_index::IndexEntry::is_skip_worktree)
            || !gitlink_should_recurse(worktree_root, &config, path)
        {
            continue;
        }
        if let Err(error) =
            checkout_submodule_to_commit(worktree_root, git_dir, format, path, &entry.oid)
        {
            // The superproject worktree has already moved, but a recursive
            // submodule failure leaves its index at the pre-reset state. The
            // reset caller refreshes that restored index against the rewritten
            // worktree before propagating the failure, matching Git's partial
            // failure contract.
            match previous_index.as_ref() {
                Some(bytes) => fs::write(&index_path, bytes)?,
                None => match fs::remove_file(&index_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.into()),
                },
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Perform git's trivial fast-forward / two-way / three-way merge of the listed
/// trees through the shared [`sley_unpack_trees`] engine, producing the
/// resulting (possibly multi-stage) index entries. With `update_worktree`, the
/// engine's computed worktree plan (removals + resolved-blob writes) is applied
/// before the entries are returned.
///
/// The number of trees selects the merge function:
/// * 1 tree  — `oneway_merge`: fast-forward, take the tree wholesale.
/// * 2 trees — `twoway_merge`: switch `old` → `new`, carry forward local adds.
/// * 3+ trees — `threeway_merge`: trivial 3-way, recording stage 1/2/3 on a
///   non-trivial path. Extra leading trees are additional merge bases.
fn merge_trees(
    worktree_root: Option<&Path>,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    tree_oids: &[ObjectId],
    update_worktree: bool,
    index_only: bool,
    sparse_checkout: bool,
    recurse_submodules: bool,
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    use sley_unpack_trees::{MergeFn, UnpackTreesOptions, check_updates, unpack_trees};

    let merge_fn = match tree_oids.len() {
        1 => MergeFn::OneWay,
        2 => MergeFn::TwoWay,
        3..=sley_unpack_trees::MAX_UNPACK_TREES => MergeFn::ThreeWay,
        0 => {
            eprintln!("fatal: you must specify at least one tree to merge");
            return Err(GitError::Exit(128));
        }
        _ => {
            eprintln!(
                "fatal: I cannot read more than {} trees",
                sley_unpack_trees::MAX_UNPACK_TREES
            );
            return Err(GitError::Exit(128));
        }
    };

    let mut index = sley_worktree::read_current_unpack_index(git_dir, format)?;
    let mut diagnostics = CliReadTreeDiagnostics;
    let trees: Vec<sley_unpack_trees::FlatTree> = tree_oids
        .iter()
        .map(|oid| {
            map_read_tree_transition_result(sley_worktree::flatten_validated_read_tree_source(
                db,
                format,
                config,
                oid,
                &mut diagnostics,
            ))
        })
        .collect::<Result<_>>()?;

    let mut sparse_paths = index.keys().cloned().collect::<BTreeSet<_>>();
    for tree in &trees {
        sparse_paths.extend(tree.keys().cloned());
    }
    let apply_sparse_checkout = sparse_checkout
        && sley_worktree::configure_active_sparse_checkout_for_unpack_index(
            git_dir,
            &mut index,
            &sparse_paths,
        )?;

    let mut opts = UnpackTreesOptions::new(format);
    opts.merge = true;
    opts.update = update_worktree;
    // git's read-tree: `o.initial_checkout = is_index_unborn(o->src_index)` — an
    // empty (unborn) index means there is no staged deletion to honour, so
    // twoway_merge must take a path from the new tree (`merged_entry`) rather
    // than its "deletion of the path was staged" arm dropping it. Almost every
    // t1001/t1002 test does `rm .git/index && read-tree -m`, so this gate is what
    // makes the post-reset merge populate the index at all.
    opts.initial_checkout = index.is_empty();
    // `read-tree -m` is index-only unless `-u` is given; the engine's worktree
    // safety checks (verify_uptodate / verify_absent) only run when not
    // index-only, matching upstream where `-m` without `-u` still runs the
    // up-to-date checks. read-tree's historic behaviour DOES run verify_uptodate
    // even without `-u`, so keep `index_only` false and let `update` gate the
    // verify_absent (clobber) check inside merged_entry.
    // `-i` (git's `index_only`): skip the worktree verify_uptodate/verify_absent
    // checks entirely. This is also what makes `read-tree -i -m` usable in a bare
    // repository, where there is no worktree to require.
    opts.index_only = index_only;
    opts.apply_sparse_checkout = apply_sparse_checkout && update_worktree && !index_only;

    let worktree_root = worktree_root
        .map(Path::to_path_buf)
        .or_else(|| index_only.then(|| git_dir.to_path_buf()))
        .ok_or_else(|| {
            GitError::Unsupported("read-tree currently requires a non-bare worktree".into())
        })?;
    let tree_attributes = tree_oids
        .last()
        .map(|tree| {
            sley_worktree::TreeAttributes::from_tree(&worktree_root, git_dir, db, format, tree)
        })
        .transpose()?;
    let mut wt = ReadTreeWorktree {
        submodules: load_superproject_submodules(&worktree_root),
        repo_config: read_repo_config(git_dir).unwrap_or_default(),
        tree_attributes,
        worktree_root,
        git_dir: git_dir.to_path_buf(),
        db,
        format,
        original_paths: sley_worktree::read_current_index_paths(git_dir, format)?,
        porcelain: UnpackPorcelain::ReadTree,
        recurse_submodules,
        force_overwrite_tracked: false,
        hooks: SubmoduleHooks {
            checkout_to_commit: Some(&checkout_submodule_to_commit),
            remove_worktree: Some(&remove_submodule_worktree),
        },
    };

    let mut result = unpack_trees(&index, &trees, merge_fn, &opts, &wt)?;

    if update_worktree {
        refuse_if_unpack_result_removes_current_directory(&wt.worktree_root, &result)?;
        // check_updates folds the post-write `lstat` back into `result.entries`
        // (git's refresh_cache), so the serialized index records real stat-info
        // for every freshly-written path.
        check_updates(&mut result, &opts, &mut wt)?;
    }

    Ok(result
        .entries
        .into_iter()
        .map(|e| {
            (
                e.path,
                StagedEntry {
                    mode: e.entry.mode,
                    oid: e.entry.oid,
                    stage: e.entry.stage,
                    stat: e.entry.stat,
                    skip_worktree: Some(e.entry.is_skip_worktree()),
                },
            )
        })
        .collect())
}

/// Materialize newly-introduced blobs into the working tree (used by
/// `--prefix -u`): only stage-0 paths whose `(mode, oid)` differ from the prior
/// index entry are written, so unrelated locally-modified files the prefix read
/// merely carried over are left untouched. Nothing is removed. The post-write
/// `lstat` is folded back into each written entry's `stat` so the serialized
/// index records real stat-info (git's `refresh_cache`).
fn update_worktree_for_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    tree_attributes: Option<&sley_worktree::TreeAttributes>,
    entries: &mut [(Vec<u8>, StagedEntry)],
    recurse_submodules: bool,
) -> Result<()> {
    let original = sley_worktree::read_current_index_stage_zero(git_dir, format)?;
    let mut written: BTreeMap<Vec<u8>, Option<sley_unpack_trees::StatInfo>> = BTreeMap::new();
    // Borrow-split: pick write order from a read-only view, then mutate by path.
    let plan: Vec<(Vec<u8>, u32, ObjectId)> = worktree_write_order(entries)
        .into_iter()
        .filter(|(_, entry)| entry.stage == 0)
        .filter(|(path, entry)| original.get(*path) != Some(&(entry.mode, entry.oid)))
        .map(|(path, entry)| (path.clone(), entry.mode, entry.oid))
        .collect();
    for (path, mode, oid) in plan {
        let stat = write_tree_entry_to_worktree(
            worktree_root,
            git_dir,
            format,
            db,
            config,
            tree_attributes,
            &path,
            mode,
            &oid,
            recurse_submodules,
        )?;
        written.insert(path, stat);
    }
    for (path, entry) in entries.iter_mut() {
        if let Some(stat) = written.get(path) {
            entry.stat = *stat;
        }
    }
    Ok(())
}

/// Reset the working tree to exactly the given stage-0 entries (`--reset -u`):
/// remove tracked files no longer present, then write each entry's blob,
/// folding the post-write `lstat` back into the entry's `stat` so the serialized
/// index is stat-accurate (git's `refresh_cache`, satisfying a follow-up
/// `diff-files`/`check_cache_at` "is it dirty?" query).
fn reset_worktree_to_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    tree_attributes: Option<&sley_worktree::TreeAttributes>,
    entries: &mut [(Vec<u8>, StagedEntry)],
    recurse_submodules: bool,
) -> Result<()> {
    refuse_if_unpack_entries_turn_cwd_into_file(worktree_root, entries)?;
    let target: BTreeSet<&Vec<u8>> = entries.iter().map(|(path, _)| path).collect();
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for entry in &index.entries {
            if !target.iter().any(|p| p.as_slice() == entry.path.as_bytes()) {
                if recurse_submodules && sley_index::is_gitlink(entry.mode) {
                    remove_submodule_worktree(worktree_root, git_dir, &entry.path)?;
                } else {
                    remove_worktree_path(worktree_root, &entry.path)?;
                }
            }
        }
    }
    let plan: Vec<(Vec<u8>, u32, ObjectId)> = worktree_write_order(entries)
        .into_iter()
        .map(|(path, entry)| (path.clone(), entry.mode, entry.oid))
        .collect();
    let mut written: BTreeMap<Vec<u8>, Option<sley_unpack_trees::StatInfo>> = BTreeMap::new();
    for (path, mode, oid) in plan {
        let stat = write_tree_entry_to_worktree(
            worktree_root,
            git_dir,
            format,
            db,
            config,
            tree_attributes,
            &path,
            mode,
            &oid,
            recurse_submodules,
        )?;
        written.insert(path, stat);
    }
    for (path, entry) in entries.iter_mut() {
        if let Some(stat) = written.get(path) {
            entry.stat = *stat;
        }
    }
    Ok(())
}

/// Order entries so each directory's `.gitattributes` is materialized before the
/// other files in that directory. The smudge filter resolves attributes from the
/// worktree `.gitattributes` first (git's default `GIT_ATTR_CHECKIN` direction),
/// so a file relying on a freshly-checked-out `.gitattributes` would otherwise
/// only see the staged fallback. Ordering by `.gitattributes`-first makes the
/// worktree copy authoritative for siblings in the same batch, matching git's
/// sorted unpack-trees materialization.
fn worktree_write_order(entries: &[(Vec<u8>, StagedEntry)]) -> Vec<(&Vec<u8>, &StagedEntry)> {
    let mut ordered: Vec<(&Vec<u8>, &StagedEntry)> =
        entries.iter().map(|(path, entry)| (path, entry)).collect();
    // Stable sort with a key that ranks a directory's `.gitattributes` ahead of
    // its siblings while otherwise preserving the original (already sorted) order.
    ordered.sort_by(|(left, _), (right, _)| {
        attribute_priority_key(left).cmp(&attribute_priority_key(right))
    });
    ordered
}

/// Sort key placing each directory's `.gitattributes` immediately before the
/// directory's other entries: `(dir, is_not_gitattributes, basename)`.
fn attribute_priority_key(path: &[u8]) -> (Vec<u8>, u8, Vec<u8>) {
    let (dir, base) = match path.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (path[..slash].to_vec(), &path[slash + 1..]),
        None => (Vec::new(), path),
    };
    let is_not_attributes = u8::from(base != b".gitattributes");
    (dir, is_not_attributes, base.to_vec())
}

pub(crate) fn checkout_submodule_to_commit(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    path: &[u8],
    oid: &ObjectId,
) -> Result<()> {
    let Some(sub_root) = safe_worktree_path(worktree_root, path) else {
        return Ok(());
    };
    let path_str = String::from_utf8_lossy(path);
    // git's `validate_submodule_path`: any path component that is a symlink
    // (including the leaf) is fatal. Following the leaf would migrate/delete
    // a linked repo's gitdir (t7423).
    if let Ok(meta) = fs::symlink_metadata(&sub_root)
        && meta.file_type().is_symlink()
    {
        eprintln!("error: expected submodule path '{path_str}' not to be a symbolic link");
        return Err(GitError::Exit(128));
    }
    if sley_submodule::submodule_path_has_symlink_parent(worktree_root, Path::new(&*path_str))? {
        eprintln!("error: expected submodule path '{path_str}' not to be a symbolic link");
        return Err(GitError::Exit(128));
    }
    let (submodule_name, submodule_url) = submodule_name_and_url_for_path(worktree_root, &path_str)
        .unwrap_or_else(|| (path_str.to_string(), None));
    let embedded_git_dir = sub_root.join(".git");
    let admin_git_dir = submodule_admin_git_dir(git_dir, &submodule_name);
    // Older/native-add repositories may still carry an embedded `.git`
    // directory even when an admin copy also exists. Use that live repository
    // directly; trying to replace the directory with a gitfile would fail with
    // EISDIR and prevent recursive checkout from reaching the child engine.
    let sub_git_dir = if embedded_git_dir.is_dir() {
        embedded_git_dir.clone()
    } else {
        admin_git_dir
    };
    if !sub_git_dir.is_dir() {
        if sub_root.join(".git").is_dir() {
            copy_dir_recursive(&sub_root.join(".git"), &sub_git_dir)?;
        } else if let Some(url) = submodule_url {
            clone_submodule_for_checkout(worktree_root, git_dir, &sub_root, &sub_git_dir, &url)?;
        } else {
            eprintln!("fatal: could not get a repository handle for submodule '{path_str}'");
            return Err(GitError::Exit(128));
        }
    }

    if fs::symlink_metadata(&sub_root).is_ok_and(|metadata| !metadata.is_dir()) {
        remove_path_in_the_way(&sub_root)?;
    }
    fs::create_dir_all(&sub_root)?;
    if sub_git_dir != embedded_git_dir {
        connect_submodule_worktree(&sub_root, &sub_git_dir)?;
    }

    let sub_format = repository_object_format(&sub_git_dir).unwrap_or(format);
    if let Err(_err) =
        sley_worktree::reset_index_and_worktree_to_commit(&sub_root, &sub_git_dir, sub_format, oid)
    {
        eprintln!("fatal: Unable to checkout '{oid}' in submodule path '{path_str}'");
        return Err(GitError::Exit(128));
    }
    fs::write(sub_git_dir.join("HEAD"), format!("{oid}\n"))?;

    if let Some(nested) = load_superproject_submodules(&sub_root)
        && let Ok(index) = sley_worktree::read_repository_index(&sub_git_dir, sub_format)
    {
        for entry in index
            .into_iter()
            .flat_map(|index| index.entries)
            .filter(|entry| {
                entry.stage() == sley_index::Stage::Normal && sley_index::is_gitlink(entry.mode)
            })
        {
            let nested_path = entry.path.as_bytes();
            let Ok(nested_path_str) = std::str::from_utf8(nested_path) else {
                continue;
            };
            if nested.from_path(nested_path_str).is_some() {
                checkout_submodule_to_commit(
                    &sub_root,
                    &sub_git_dir,
                    sub_format,
                    nested_path,
                    &entry.oid,
                )?;
            }
        }
    }
    Ok(())
}

/// Reconcile populated submodules selected by a superproject path checkout.
///
/// The superproject index owns the target gitlink OID; each child uses the
/// same worktree reset engine, including its configured materialization queue.
pub(crate) fn checkout_submodules_for_paths(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<()> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(());
    };
    let configured = load_superproject_submodules(worktree_root);
    for entry in index.entries.into_iter().filter(|entry| {
        entry.stage() == sley_index::Stage::Normal && sley_index::is_gitlink(entry.mode)
    }) {
        let git_path = entry.path.as_bytes();
        let Ok(path_str) = std::str::from_utf8(git_path) else {
            continue;
        };
        if configured
            .as_ref()
            .and_then(|set| set.from_path(path_str))
            .is_none()
        {
            continue;
        }
        let full = worktree_root.join(path_str);
        let selected = paths.iter().any(|requested| {
            requested == worktree_root
                || full == *requested
                || full.starts_with(requested)
                || requested.starts_with(&full)
        });
        if selected {
            checkout_submodule_to_commit(worktree_root, git_dir, format, git_path, &entry.oid)?;
        }
    }
    Ok(())
}

fn submodule_name_and_url_for_path(
    worktree_root: &Path,
    path: &str,
) -> Option<(String, Option<String>)> {
    let config = GitConfig::read(worktree_root.join(".gitmodules")).ok()?;
    let set = sley_submodule::SubmoduleConfigSet::parse(&config);
    let submodule = set.from_path(path)?;
    Some((submodule.name.clone(), submodule.url.clone()))
}

fn clone_submodule_for_checkout(
    worktree_root: &Path,
    git_dir: &Path,
    sub_root: &Path,
    sub_git_dir: &Path,
    url: &str,
) -> Result<()> {
    let config = read_repo_config(git_dir).unwrap_or_default();
    let base = config.get("remote", Some("origin"), "url");
    let fallback = worktree_root.to_string_lossy();
    let resolved = sley_submodule::resolve_relative_url(url, base, &fallback, None);
    let args = vec![
        "--separate-git-dir".to_string(),
        sub_git_dir.display().to_string(),
        resolved,
        sub_root.display().to_string(),
    ];
    let clone_session = crate::session::CliSession::isolated_child(worktree_root.to_path_buf());
    super::remote::cmd_clone(&clone_session, &args)?;
    connect_submodule_worktree(sub_root, sub_git_dir)?;
    Ok(())
}

fn remove_submodule_worktree(worktree_root: &Path, git_dir: &Path, path: &[u8]) -> Result<()> {
    let Some(sub_root) = safe_worktree_path(worktree_root, path) else {
        return Ok(());
    };
    let path_str = String::from_utf8_lossy(path);
    // Refuse through a symlinked parent *or* when the submodule path itself is
    // a symlink: following the leaf would migrate/delete the linked repo's
    // gitdir (t7423: checkout -f --recurse-submodules must not migrate gitdir
    // of a symlinked repo when removing the submodule).
    if sley_submodule::submodule_path_has_symlink_parent(worktree_root, Path::new(&*path_str))?
        || fs::symlink_metadata(&sub_root)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
    {
        eprintln!("fatal: refusing to remove submodule path '{path_str}' through a symlink");
        return Err(GitError::Exit(128));
    }
    let sub_git_dir = submodule_admin_git_dir(git_dir, &path_str);
    if sub_root.join(".git").is_dir() && !sub_git_dir.is_dir() {
        copy_dir_recursive(&sub_root.join(".git"), &sub_git_dir)?;
    }
    if sub_root.exists() {
        fs::remove_dir_all(&sub_root)?;
    }
    unset_core_worktree_recursive(&sub_git_dir)?;
    prune_empty_dirs(worktree_root, sub_root.parent());
    Ok(())
}

fn submodule_admin_git_dir(super_git_dir: &Path, name: &str) -> PathBuf {
    let mut path = super_git_dir.join("modules");
    for component in name.split('/') {
        if !component.is_empty() {
            path.push(component);
        }
    }
    path
}

fn connect_submodule_worktree(sub_root: &Path, sub_git_dir: &Path) -> Result<()> {
    fs::create_dir_all(sub_git_dir)?;
    fs::write(
        sub_root.join(".git"),
        format!("gitdir: {}\n", sub_git_dir.display()),
    )?;
    crate::commands::submodule::set_submodule_core_worktree(sub_root, sub_git_dir)?;
    Ok(())
}

fn unset_core_worktree(git_dir: &Path) -> Result<()> {
    let config = git_dir.join("config");
    let Ok(mut text) = fs::read_to_string(&config) else {
        return Ok(());
    };
    remove_config_key_in_place(&mut text, "core", "worktree");
    fs::write(config, text)?;
    Ok(())
}

fn unset_core_worktree_recursive(git_dir: &Path) -> Result<()> {
    unset_core_worktree(git_dir)?;
    let modules = git_dir.join("modules");
    let Ok(entries) = fs::read_dir(&modules) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            unset_core_worktree_recursive(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_config_key_in_place(text: &mut String, section: &str, key: &str) {
    let mut current_section = String::new();
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('"')
                .to_ascii_lowercase();
        }
        let line_key = trimmed
            .split_once('=')
            .map(|(left, _)| left.trim().to_ascii_lowercase());
        if current_section == section && line_key.as_deref() == Some(key) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    *text = out;
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
