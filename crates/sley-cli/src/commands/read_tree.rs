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

use crate::*;

/// A flat map from a repository-relative path to a tree leaf's `(mode, oid)`.
type LeafMap = BTreeMap<Vec<u8>, (u32, ObjectId)>;

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
    empty: bool,
    trees: Vec<String>,
}

/// A single resolved index entry destined for stage 0/1/2/3.
#[derive(Debug, Clone)]
struct StagedEntry {
    mode: u32,
    oid: ObjectId,
    stage: u8,
}

pub(crate) fn cmd_read_tree(args: &[String]) -> Result<()> {
    let parsed = parse_read_tree_args(args)?;

    let repo = RepositoryContext::discover_current()?;
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();

    // `--empty` (and the deprecated no-argument spelling) just clears the index.
    if parsed.empty {
        write_paired_entries(git_dir, format, Vec::new())?;
        return Ok(());
    }

    // Resolve every positional argument to a tree object id up front so an
    // invalid tree-ish aborts before any index mutation (matching git).
    let mut tree_oids = Vec::with_capacity(parsed.trees.len());
    for tree in &parsed.trees {
        tree_oids.push(resolve_tree_ish(&repo, tree)?);
    }

    match &parsed.mode {
        ReadTreeMode::Read => {
            let entries = read_tree_overlay(db, format, &tree_oids)?;
            write_paired_entries(git_dir, format, entries)?;
        }
        ReadTreeMode::Reset => {
            // `--reset` accepts up to three trees but only the resulting union
            // matters; higher-stage entries are simply dropped (we never create
            // them here). With `-u` the worktree is updated to match.
            let entries = read_tree_overlay(db, format, &tree_oids)?;
            if parsed.update_worktree {
                let worktree_root = worktree_root_for_git_dir(git_dir)?;
                reset_worktree_to_entries(&worktree_root, git_dir, format, db, &entries)?;
            }
            write_paired_entries(git_dir, format, entries)?;
        }
        ReadTreeMode::Prefix(prefix) => {
            let entries = read_tree_prefix(git_dir, format, db, &tree_oids, prefix)?;
            if parsed.update_worktree {
                let worktree_root = worktree_root_for_git_dir(git_dir)?;
                update_worktree_for_entries(&worktree_root, git_dir, format, db, &entries)?;
            }
            write_paired_entries(git_dir, format, entries)?;
        }
        ReadTreeMode::Merge => {
            // The original index paths distinguish "added" entries (which `-u`
            // must not write over an untracked file) from those already tracked.
            let original_paths = original_index_paths(git_dir, format)?;
            let entries = merge_trees(git_dir, format, db, &tree_oids)?;
            if parsed.update_worktree {
                let worktree_root = worktree_root_for_git_dir(git_dir)?;
                // git validates the whole worktree update before touching
                // anything; refuse if a freshly added path would clobber an
                // untracked file.
                verify_no_untracked_overwrites(&worktree_root, &entries, &original_paths)?;
                update_worktree_for_merge(&worktree_root, git_dir, format, db, &entries)?;
            }
            write_paired_entries(git_dir, format, entries)?;
        }
    }

    Ok(())
}

/// Parse `read-tree`'s arguments, enforcing the same mutual-exclusion and
/// arity rules as upstream git (and the matching `fatal:`/exit codes).
fn parse_read_tree_args(args: &[String]) -> Result<ReadTreeArgs> {
    let mut mode: Option<ReadTreeMode> = None;
    let mut update_worktree = false;
    let mut empty = false;
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
            "-i" => {} // "don't check the working tree" — we already skip those checks.
            "--empty" => empty = true,
            "--no-empty" => empty = false,
            "--prefix" => {
                let value = iter.next().ok_or_else(|| {
                    eprintln!("error: option `prefix' requires a value");
                    GitError::Exit(129)
                })?;
                set_mode(ReadTreeMode::Prefix(normalize_prefix(value)), &mut mode)?;
            }
            // Accepted no-op switches that don't change our deterministic output.
            "-v"
            | "--verbose"
            | "--no-verbose"
            | "-q"
            | "--quiet"
            | "--no-quiet"
            | "--trivial"
            | "--no-trivial"
            | "--aggressive"
            | "--no-aggressive"
            | "--no-sparse-checkout"
            | "--sparse-checkout"
            | "--debug-unpack"
            | "--no-debug-unpack"
            | "-n"
            | "--dry-run"
            | "--no-dry-run" => {}
            value => {
                if let Some(prefix) = value.strip_prefix("--prefix=") {
                    set_mode(ReadTreeMode::Prefix(normalize_prefix(prefix)), &mut mode)?;
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
            empty,
            trees,
        });
    }

    validate_read_tree_arity(&mode, update_worktree, &trees)?;

    Ok(ReadTreeArgs {
        mode,
        update_worktree,
        empty,
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
            if trees.len() > 3 {
                eprintln!("fatal: too many trees given for read-tree");
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(())
}

/// Normalize a `--prefix` value to git's canonical form: a non-empty path that
/// ends in a single `/` (an empty prefix becomes the root sentinel `/`).
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

/// Recursively collect a tree's blob/symlink/gitlink leaves into `out`,
/// keyed by full path. Subtrees are descended; the path separator is `/`.
fn collect_tree_leaves(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &[u8],
    out: &mut LeafMap,
) -> Result<()> {
    // The empty tree contributes no leaves and need not be stored.
    if tree_oid == &empty_tree_oid(format)? {
        return Ok(());
    }
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let tree = Tree::parse(format, &object.body)?;
    for entry in tree.entries {
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        if entry.mode == 0o040000 {
            collect_tree_leaves(db, format, &entry.oid, &path, out)?;
        } else {
            out.insert(path, (entry.mode, entry.oid));
        }
    }
    Ok(())
}

/// Read one tree into a flat path -> (mode, oid) map.
fn tree_leaf_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<LeafMap> {
    let mut out = BTreeMap::new();
    collect_tree_leaves(db, format, tree_oid, &[], &mut out)?;
    Ok(out)
}

/// Convert a leaf map to sorted stage-0 `(path, entry)` pairs.
fn leaves_to_stage0(leaves: LeafMap) -> Vec<(Vec<u8>, StagedEntry)> {
    leaves
        .into_iter()
        .map(|(path, (mode, oid))| (path, stage0(mode, oid)))
        .collect()
}

/// Overlay the listed trees into a single stage-0 index, with later trees
/// winning on path collisions (git's non-merge multi-tree read).
fn read_tree_overlay(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oids: &[ObjectId],
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    let mut merged: LeafMap = BTreeMap::new();
    for tree_oid in tree_oids {
        for (path, value) in tree_leaf_map(db, format, tree_oid)? {
            merged.insert(path, value);
        }
    }
    Ok(leaves_to_stage0(merged))
}

/// Overlay a single tree (or up to three, last-wins) under `prefix` into the
/// *current* index (stage 0), keeping every existing entry; on an exact path
/// collision the new tree wins.
fn read_tree_prefix(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    tree_oids: &[ObjectId],
    prefix: &[u8],
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    let mut merged: LeafMap = current_index_stage0(git_dir, format)?;

    // `prefix` is normalized to end in `/`; the root prefix is the sentinel
    // "/", which means "no path prefix".
    let real_prefix: &[u8] = if prefix == b"/" { b"" } else { prefix };

    for tree_oid in tree_oids {
        for (path, value) in tree_leaf_map(db, format, tree_oid)? {
            let mut full = real_prefix.to_vec();
            full.extend_from_slice(&path);
            merged.insert(full, value);
        }
    }

    Ok(leaves_to_stage0(merged))
}

/// Read the current on-disk index's stage-0 entries into a path -> (mode, oid)
/// map (used as the base for `--prefix` and the merge "current" side). A missing
/// index yields an empty map.
fn current_index_stage0(git_dir: &Path, format: ObjectFormat) -> Result<LeafMap> {
    let mut out = BTreeMap::new();
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for entry in index.entries {
            if entry_stage(&entry) == 0 {
                out.insert(entry.path, (entry.mode, entry.oid));
            }
        }
    }
    Ok(out)
}

/// Extract the merge stage (0-3) encoded in bits 12-13 of an index entry's
/// `flags` field.
fn entry_stage(entry: &IndexEntry) -> u8 {
    ((entry.flags >> 12) & 0x3) as u8
}

/// The set of every path present in the current index, across all stages. Used
/// to tell which merge-result entries are newly added (and so subject to the
/// "would be overwritten" untracked-file check under `-u`).
fn original_index_paths(git_dir: &Path, format: ObjectFormat) -> Result<BTreeSet<Vec<u8>>> {
    let mut out = BTreeSet::new();
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for entry in index.entries {
            out.insert(entry.path);
        }
    }
    Ok(out)
}

/// Refuse a `-m -u` worktree update that would write a *newly added* path on top
/// of an existing untracked file, mirroring git's pre-flight check (which aborts
/// the whole operation, reporting the first offending path in sorted order).
fn verify_no_untracked_overwrites(
    worktree_root: &Path,
    entries: &[(Vec<u8>, StagedEntry)],
    original_paths: &BTreeSet<Vec<u8>>,
) -> Result<()> {
    // `entries` is already sorted by path, so the first hit matches git.
    for (path, entry) in entries {
        if entry.stage != 0 || original_paths.contains(path) {
            continue;
        }
        let Some(file_path) = safe_worktree_path(worktree_root, path) else {
            continue;
        };
        // Re-materializing a path we already track is fine; an untracked file in
        // the way is not. `original_paths` excluded tracked paths above, so any
        // existing file here is untracked.
        if let Ok(metadata) = fs::symlink_metadata(&file_path)
            && metadata.is_file()
        {
            let display = String::from_utf8_lossy(path);
            eprintln!(
                "error: Untracked working tree file '{display}' would be overwritten by merge."
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

/// Perform git's trivial fast-forward / two-way / three-way merge of the listed
/// trees, producing the resulting (possibly multi-stage) index entries.
///
/// The number of trees selects the flavour:
/// * 1 tree  — fast-forward: take the tree wholesale into stage 0.
/// * 2 trees — two-way merge (`old`, `new`); take `new`, removing paths absent
///   from `new`. Paths whose worktree/index copy is not up to date abort.
/// * 3 trees — three-way merge (`base`, `ours`, `theirs`) using the documented
///   trivial-merge stage rules.
fn merge_trees(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    tree_oids: &[ObjectId],
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    match tree_oids {
        [tree] => merge_one_tree(git_dir, format, db, tree),
        [old, new] => merge_two_trees(git_dir, format, db, old, new),
        [base, ours, theirs] => merge_three_trees(git_dir, format, db, base, ours, theirs),
        _ => {
            eprintln!("fatal: you must specify at least one tree to merge");
            Err(GitError::Exit(128))
        }
    }
}

/// One-tree merge (`-m <tree>`): fast-forward the index to `tree`. Paths the
/// merge changes or removes must be up to date in the worktree.
fn merge_one_tree(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    tree: &ObjectId,
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let current = current_index_stage0(git_dir, format)?;
    let target = tree_leaf_map(db, format, tree)?;

    let mut all_paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    all_paths.extend(current.keys());
    all_paths.extend(target.keys());
    for path in all_paths {
        if current.get(path) != target.get(path) {
            verify_uptodate(&worktree_root, db, format, path, current.get(path))?;
        }
    }

    Ok(leaves_to_stage0(target))
}

/// Two-tree merge (`-m <old> <new>`): switch the index from `old` to `new`,
/// keeping local additions present in the index but absent from `old`.
fn merge_two_trees(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    old: &ObjectId,
    new: &ObjectId,
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let old_map = tree_leaf_map(db, format, old)?;
    let new_map = tree_leaf_map(db, format, new)?;
    let index = current_index_stage0(git_dir, format)?;

    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(old_map.keys());
    paths.extend(new_map.keys());
    paths.extend(index.keys());

    let mut result: Vec<(Vec<u8>, StagedEntry)> = Vec::new();
    for path in paths {
        let in_old = old_map.get(path);
        let in_new = new_map.get(path);
        let in_index = index.get(path);

        match (in_old, in_new) {
            (Some(o), Some(n)) => {
                if o == n {
                    // Unchanged old->new: keep the index copy if present, else
                    // take the (identical) tree copy.
                    let (mode, oid) = in_index.cloned().unwrap_or_else(|| n.clone());
                    result.push((path.clone(), stage0(mode, oid)));
                } else {
                    // Changed old->new: the working tree must be up to date,
                    // then we take `new`.
                    verify_uptodate(&worktree_root, db, format, path, in_index)?;
                    let (mode, oid) = n.clone();
                    result.push((path.clone(), stage0(mode, oid)));
                }
            }
            (Some(_o), None) => {
                // Removed in new: drop it, but the worktree must be up to date.
                verify_uptodate(&worktree_root, db, format, path, in_index)?;
            }
            (None, Some(n)) => {
                // Added in new: keep an identical index copy, else require the
                // path be up to date before writing the new content.
                if in_index != Some(n) {
                    verify_uptodate(&worktree_root, db, format, path, in_index)?;
                }
                let (mode, oid) = n.clone();
                result.push((path.clone(), stage0(mode, oid)));
            }
            (None, None) => {
                // Only present locally (a local addition): keep it.
                if let Some((mode, oid)) = in_index {
                    result.push((path.clone(), stage0(*mode, oid.clone())));
                }
            }
        }
    }
    result.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(result)
}

/// Three-tree merge (`-m <base> <ours> <theirs>`): the documented trivial
/// three-way merge that resolves what it can to stage 0 and otherwise records
/// stage 1 (base) / 2 (ours) / 3 (theirs) entries.
fn merge_three_trees(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    base: &ObjectId,
    ours: &ObjectId,
    theirs: &ObjectId,
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let base_map = tree_leaf_map(db, format, base)?;
    let ours_map = tree_leaf_map(db, format, ours)?;
    let theirs_map = tree_leaf_map(db, format, theirs)?;
    let index = current_index_stage0(git_dir, format)?;

    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(base_map.keys());
    paths.extend(ours_map.keys());
    paths.extend(theirs_map.keys());

    let mut result: Vec<(Vec<u8>, StagedEntry)> = Vec::new();
    for path in paths {
        let o = base_map.get(path);
        let a = ours_map.get(path);
        let b = theirs_map.get(path);
        let in_index = index.get(path);

        // Trivial resolutions to stage 0 (see git-read-tree(1) trivial rules):
        //   * ours == theirs  -> take it (both sides agree, incl. both-removed)
        //   * theirs == base  -> take ours   (only ours changed)
        //   * ours == base    -> take theirs (only theirs changed)
        let resolved: Option<&(u32, ObjectId)> = if a == b || b == o {
            // Both sides agree, or theirs is unchanged from base: take ours.
            a
        } else if a == o {
            // Only theirs changed: take theirs.
            b
        } else {
            None
        };

        match resolved {
            Some(value) => {
                // A clean resolution. If it changes the working-tree side, the
                // path must be up to date first.
                if Some(value) != in_index {
                    verify_uptodate(&worktree_root, db, format, path, in_index)?;
                }
                let (mode, oid) = value.clone();
                result.push((path.clone(), stage0(mode, oid)));
            }
            None => {
                // No trivial resolution: emit the surviving stages. Before
                // recording a conflict the path must be up to date so we do not
                // silently clobber local work.
                verify_uptodate(&worktree_root, db, format, path, in_index)?;
                if let Some((mode, oid)) = o {
                    result.push((path.clone(), staged(*mode, oid.clone(), 1)));
                }
                if let Some((mode, oid)) = a {
                    result.push((path.clone(), staged(*mode, oid.clone(), 2)));
                }
                if let Some((mode, oid)) = b {
                    result.push((path.clone(), staged(*mode, oid.clone(), 3)));
                }
            }
        }
    }
    result.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.stage.cmp(&right.1.stage))
    });
    Ok(result)
}

/// Construct a stage-0 [`StagedEntry`].
fn stage0(mode: u32, oid: ObjectId) -> StagedEntry {
    StagedEntry {
        mode,
        oid,
        stage: 0,
    }
}

/// Construct a [`StagedEntry`] at an explicit stage.
fn staged(mode: u32, oid: ObjectId, stage: u8) -> StagedEntry {
    StagedEntry { mode, oid, stage }
}

/// Abort with git's "not uptodate" error when the working-tree copy of `path`
/// disagrees with the index entry the merge is about to replace/remove.
///
/// `expected` is the index content the merge assumes (the stage-0 entry, or
/// `None` if the path is currently untracked). The worktree file must hash to
/// the same blob for the operation to be safe; a missing tracked file is
/// considered up to date (git permits re-materializing it).
fn verify_uptodate(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    path: &[u8],
    expected: Option<&(u32, ObjectId)>,
) -> Result<()> {
    let Some((_mode, expected_oid)) = expected else {
        // Untracked path: nothing in the index to be out of date with.
        return Ok(());
    };
    let _ = db;
    let Some(file_path) = safe_worktree_path(worktree_root, path) else {
        return Ok(());
    };
    let body = match fs::read(&file_path) {
        Ok(body) => body,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let actual = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    if &actual != expected_oid {
        let display = String::from_utf8_lossy(path);
        eprintln!("error: Entry '{display}' not uptodate. Cannot merge.");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// Join a repository-relative path onto `root`, rejecting absolute or
/// parent-escaping components (returns `None` so callers can skip safely).
fn safe_worktree_path(root: &Path, path: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(path).ok()?;
    let relative = PathBuf::from(text);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(root.join(relative))
}

/// Build and persist the on-disk index from sorted `(path, entry)` pairs.
fn write_paired_entries(
    git_dir: &Path,
    format: ObjectFormat,
    pairs: Vec<(Vec<u8>, StagedEntry)>,
) -> Result<()> {
    let mut index_entries = Vec::with_capacity(pairs.len());
    for (path, entry) in pairs {
        index_entries.push(make_index_entry(path, entry)?);
    }
    index_entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags & 0x3000).cmp(&(right.flags & 0x3000)))
    });
    persist_index(git_dir, format, index_entries)
}

/// Convert a `(path, StagedEntry)` into a writable [`IndexEntry`], encoding the
/// stage into bits 12-13 of `flags` and the path length into the low 12 bits.
fn make_index_entry(path: Vec<u8>, entry: StagedEntry) -> Result<IndexEntry> {
    let name_len = path.len().min(0x0fff) as u16;
    let stage_bits = ((entry.stage as u16) & 0x3) << 12;
    Ok(IndexEntry {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        dev: 0,
        ino: 0,
        mode: entry.mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid: entry.oid,
        flags: name_len | stage_bits,
        flags_extended: 0,
        path,
    })
}

/// Serialize `entries` into the repository index file. Stage > 0 entries set the
/// stage bits in `flags`; the index v2/v3 writer accepts those (the higher bits
/// of `flags`), so a fixed version 2 layout matches git's `ls-files --stage`.
fn persist_index(git_dir: &Path, format: ObjectFormat, entries: Vec<IndexEntry>) -> Result<()> {
    let index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    let bytes = index.write(format)?;
    fs::write(sley_worktree::repository_index_path(git_dir), bytes)?;
    Ok(())
}

/// Materialize newly-introduced blobs into the working tree (used by
/// `--prefix -u`): only stage-0 paths whose `(mode, oid)` differ from the prior
/// index entry are written, so unrelated locally-modified files the prefix read
/// merely carried over are left untouched. Nothing is removed.
fn update_worktree_for_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    entries: &[(Vec<u8>, StagedEntry)],
) -> Result<()> {
    let original = current_index_stage0(git_dir, format)?;
    for (path, entry) in entries {
        if entry.stage != 0 {
            continue;
        }
        if original.get(path) == Some(&(entry.mode, entry.oid.clone())) {
            continue;
        }
        write_blob_to_worktree(worktree_root, db, path, &entry.oid)?;
    }
    Ok(())
}

/// Reset the working tree to exactly the given stage-0 entries (`--reset -u`):
/// remove tracked files no longer present, then write each entry's blob.
fn reset_worktree_to_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    entries: &[(Vec<u8>, StagedEntry)],
) -> Result<()> {
    let target: BTreeSet<&Vec<u8>> = entries.iter().map(|(path, _)| path).collect();
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for entry in &index.entries {
            if !target.contains(&entry.path) {
                remove_worktree_path(worktree_root, &entry.path)?;
            }
        }
    }
    for (path, entry) in entries {
        write_blob_to_worktree(worktree_root, db, path, &entry.oid)?;
    }
    Ok(())
}

/// Apply a completed merge result to the working tree (`-m -u`): remove tracked
/// paths the merge dropped, and write resolved (stage-0) blobs whose content
/// actually changed. Conflicted (stage > 0) paths are left in the working tree
/// untouched, matching git.
///
/// git's `-u` is a *minimal* update: a path whose merged stage-0 entry equals
/// the pre-merge index entry is left on disk as-is (so a locally deleted file
/// the merge did not touch stays deleted). This reads the still-unwritten index,
/// which therefore reflects the original (pre-merge) state.
fn update_worktree_for_merge(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    entries: &[(Vec<u8>, StagedEntry)],
) -> Result<()> {
    let kept: BTreeSet<&Vec<u8>> = entries.iter().map(|(path, _)| path).collect();
    let original = current_index_stage0(git_dir, format)?;
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for entry in &index.entries {
            if !kept.contains(&entry.path) {
                remove_worktree_path(worktree_root, &entry.path)?;
            }
        }
    }
    for (path, entry) in entries {
        if entry.stage != 0 {
            continue;
        }
        // Skip paths the merge left identical to the prior index entry.
        if original.get(path) == Some(&(entry.mode, entry.oid.clone())) {
            continue;
        }
        write_blob_to_worktree(worktree_root, db, path, &entry.oid)?;
    }
    Ok(())
}

/// Write a single blob from the object database to `path` under `worktree_root`,
/// creating parent directories as needed. Non-blob targets are rejected.
fn write_blob_to_worktree(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    path: &[u8],
    oid: &ObjectId,
) -> Result<()> {
    let Some(file_path) = safe_worktree_path(worktree_root, path) else {
        return Err(GitError::InvalidPath(format!(
            "invalid worktree path {}",
            String::from_utf8_lossy(path)
        )));
    };
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&file_path, &object.body)?;
    Ok(())
}

/// Remove a working-tree file and prune any directories left empty, ignoring an
/// already-absent target.
fn remove_worktree_path(worktree_root: &Path, path: &[u8]) -> Result<()> {
    let Some(file_path) = safe_worktree_path(worktree_root, path) else {
        return Ok(());
    };
    match fs::remove_file(&file_path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    }
    prune_empty_dirs(worktree_root, file_path.parent());
    Ok(())
}

/// Remove now-empty parent directories up to (but not including) the worktree
/// root. Errors are swallowed: a non-empty or vanished directory simply stops
/// the walk.
fn prune_empty_dirs(root: &Path, mut dir: Option<&Path>) {
    while let Some(path) = dir {
        if path == root {
            break;
        }
        if fs::remove_dir(path).is_err() {
            break;
        }
        dir = path.parent();
    }
}
