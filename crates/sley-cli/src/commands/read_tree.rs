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

/// A single resolved index entry destined for stage 0/1/2/3.
#[derive(Debug, Clone)]
struct StagedEntry {
    mode: u32,
    oid: ObjectId,
    stage: u8,
    /// git's `ce_stat_data` for this entry: carried forward from the source
    /// index on a kept entry, or filled from the post-write `lstat` after a
    /// `-u` worktree apply. `None` serializes as an all-zero stat (a fresh /
    /// not-yet-refreshed entry, which `diff-files` treats as racily clean).
    stat: Option<sley_unpack_trees::StatInfo>,
}

pub(crate) fn cmd_read_tree(args: &[String]) -> Result<()> {
    let parsed = parse_read_tree_args(args)?;

    let repo = RepositoryContext::discover_current()?;
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
        write_paired_entries(git_dir, format, Vec::new())?;
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
            let entries = read_tree_overlay(db, format, repo.config(), &tree_oids)?;
            if !parsed.dry_run {
                write_paired_entries(git_dir, format, entries)?;
                if parsed.update_worktree && parsed.sparse_checkout {
                    apply_read_tree_sparse_checkout(git_dir, format, repo.config())?;
                }
            }
        }
        ReadTreeMode::Reset => {
            // `--reset` accepts up to three trees but only the resulting union
            // matters; higher-stage entries are simply dropped (we never create
            // them here). With `-u` the worktree is updated to match.
            let mut entries = read_tree_overlay(db, format, repo.config(), &tree_oids)?;
            if apply_worktree {
                let worktree_root = worktree_root_for_git_dir(git_dir)?;
                reset_worktree_to_entries(
                    &worktree_root,
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
                write_paired_entries(git_dir, format, entries)?;
                if parsed.update_worktree && parsed.sparse_checkout {
                    apply_read_tree_sparse_checkout(git_dir, format, repo.config())?;
                }
            }
        }
        ReadTreeMode::Prefix(prefix) => {
            let mut entries =
                read_tree_prefix(git_dir, format, db, repo.config(), &tree_oids, prefix)?;
            if apply_worktree {
                let worktree_root = worktree_root_for_git_dir(git_dir)?;
                update_worktree_for_entries(
                    &worktree_root,
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
                write_paired_entries(git_dir, format, entries)?;
                if parsed.update_worktree && parsed.sparse_checkout {
                    apply_read_tree_sparse_checkout(git_dir, format, repo.config())?;
                }
            }
        }
        ReadTreeMode::Merge => {
            // The trivial fast-forward / two-way / three-way merge now runs
            // through the shared `sley-unpack-trees` engine (git's
            // oneway/twoway/threeway_merge). The engine computes the result
            // index and the worktree update plan; we apply the plan with `-u`.
            let entries = merge_trees(
                git_dir,
                format,
                db,
                repo.config(),
                &tree_oids,
                apply_worktree,
                parsed.index_only,
                recurse_submodules,
            )?;
            if !parsed.dry_run {
                write_paired_entries(git_dir, format, entries)?;
                if parsed.update_worktree && parsed.sparse_checkout {
                    apply_read_tree_sparse_checkout(git_dir, format, repo.config())?;
                }
            }
        }
    }

    Ok(())
}

fn apply_read_tree_sparse_checkout(
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
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    sley_worktree::apply_sparse_checkout_with_mode(&worktree_root, git_dir, format, &sparse, mode)?;
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
    config: &GitConfig,
    tree_oids: &[ObjectId],
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    let path_rules = ReadTreePathRules::from_config(config);
    let mut merged: LeafMap = BTreeMap::new();
    for tree_oid in tree_oids {
        for (path, value) in sley_diff_merge::flatten_tree(db, format, tree_oid)? {
            verify_read_tree_path(&path, value.0, path_rules)?;
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
    config: &GitConfig,
    tree_oids: &[ObjectId],
    prefix: &[u8],
) -> Result<Vec<(Vec<u8>, StagedEntry)>> {
    let path_rules = ReadTreePathRules::from_config(config);
    let mut merged: LeafMap = current_index_stage0(git_dir, format)?;

    // `prefix` is normalized to end in `/`; the root prefix is the sentinel
    // "/", which means "no path prefix".
    let real_prefix: &[u8] = if prefix == b"/" { b"" } else { prefix };

    for tree_oid in tree_oids {
        for (path, value) in sley_diff_merge::flatten_tree(db, format, tree_oid)? {
            let mut full = real_prefix.to_vec();
            full.extend_from_slice(&path);
            verify_read_tree_path(&full, value.0, path_rules)?;
            merged.insert(full, value);
        }
    }

    Ok(leaves_to_stage0(merged))
}

#[derive(Debug, Clone, Copy)]
struct ReadTreePathRules {
    protect_hfs: bool,
    protect_ntfs: bool,
}

impl ReadTreePathRules {
    fn from_config(config: &GitConfig) -> Self {
        Self {
            protect_hfs: config.get_bool("core", None, "protectHFS").unwrap_or(false),
            protect_ntfs: config
                .get_bool("core", None, "protectNTFS")
                .unwrap_or(false),
        }
    }
}

/// git's `verify_path()` check for tree entries read into the index. It rejects
/// paths that would address `.git` (or its HFS/NTFS aliases), `.`/`..`, or an
/// embedded NUL before any index/worktree mutation.
fn verify_read_tree_path(path: &[u8], mode: u32, rules: ReadTreePathRules) -> Result<()> {
    if path.is_empty() || path.contains(&0) {
        return invalid_read_tree_path(path);
    }

    for component in path.split(|&byte| byte == b'/') {
        if component.is_empty() || component == b"." || component == b".." {
            return invalid_read_tree_path(path);
        }
        if component.eq_ignore_ascii_case(b".git") {
            return invalid_read_tree_path(path);
        }
        if rules.protect_hfs && is_hfs_dotgit(component) {
            return invalid_read_tree_path(path);
        }
        if rules.protect_ntfs && is_ntfs_dotgit(component) {
            return invalid_read_tree_path(path);
        }
        if mode == 0o120000 {
            if rules.protect_hfs && is_hfs_dotgitmodules(component) {
                return invalid_read_tree_path(path);
            }
            if rules.protect_ntfs && is_ntfs_dotgitmodules(component) {
                return invalid_read_tree_path(path);
            }
            if component.eq_ignore_ascii_case(b".gitmodules") {
                return invalid_read_tree_path(path);
            }
        }
    }

    Ok(())
}

fn invalid_read_tree_path(path: &[u8]) -> Result<()> {
    eprintln!("error: invalid path '{}'", String::from_utf8_lossy(path));
    Err(GitError::Exit(128))
}

fn is_hfs_dotgit(name: &[u8]) -> bool {
    strip_hfs_ignorable(name).eq_ignore_ascii_case(b".git")
}

fn is_hfs_dotgitmodules(name: &[u8]) -> bool {
    strip_hfs_ignorable(name).eq_ignore_ascii_case(b".gitmodules")
}

fn is_ntfs_dotgit(name: &[u8]) -> bool {
    for segment in name.split(|&byte| byte == b'\\') {
        let stream_name = segment
            .iter()
            .position(|&byte| byte == b':')
            .map_or(segment, |colon| &segment[..colon]);
        let mut end = stream_name.len();
        while end > 0 && (stream_name[end - 1] == b'.' || stream_name[end - 1] == b' ') {
            end -= 1;
        }
        let trimmed = &stream_name[..end];
        if trimmed.eq_ignore_ascii_case(b".git") || trimmed.eq_ignore_ascii_case(b"git~1") {
            return true;
        }
    }
    false
}

fn is_ntfs_dotgitmodules(name: &[u8]) -> bool {
    is_ntfs_dot_name(name, b".gitmodules", b"gitmodules", b"gi7eba")
}

fn is_ntfs_dot_name(name: &[u8], long: &[u8], short_base: &[u8], fallback: &[u8]) -> bool {
    for segment in name.split(|&byte| byte == b'\\') {
        let stream_name = segment
            .iter()
            .position(|&byte| byte == b':')
            .map_or(segment, |colon| &segment[..colon]);
        let mut end = stream_name.len();
        while end > 0 && (stream_name[end - 1] == b'.' || stream_name[end - 1] == b' ') {
            end -= 1;
        }
        let trimmed = &stream_name[..end];
        if trimmed.eq_ignore_ascii_case(long) {
            return true;
        }
        if short_base.len() >= 6
            && trimmed.len() == 8
            && trimmed[..6].eq_ignore_ascii_case(&short_base[..6])
            && trimmed[6] == b'~'
            && matches!(trimmed[7], b'1'..=b'4')
        {
            return true;
        }
        if trimmed.len() == 8 && trimmed[..fallback.len()].eq_ignore_ascii_case(fallback) {
            let mut saw_tilde = false;
            let mut ok = true;
            for byte in trimmed.iter().take(8).skip(fallback.len()) {
                if saw_tilde {
                    ok &= byte.is_ascii_digit();
                } else if *byte == b'~' {
                    saw_tilde = true;
                } else {
                    ok = false;
                }
            }
            if ok && saw_tilde {
                return true;
            }
        }
    }
    false
}

fn strip_hfs_ignorable(name: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(name) else {
        return name.to_vec();
    };
    text.chars()
        .filter(|ch| !is_hfs_ignorable(*ch))
        .collect::<String>()
        .into_bytes()
}

fn is_hfs_ignorable(ch: char) -> bool {
    matches!(
        ch as u32,
        0x200c | 0x200d | 0x200e | 0x200f | 0x202a..=0x202e | 0x206a..=0x206f
        | 0xfeff | 0x00ad | 0x034f | 0x115f | 0x1160 | 0x17b4 | 0x17b5 | 0x2060..=0x2064
    )
}

/// Read the current on-disk index's stage-0 entries into a path -> (mode, oid)
/// map (used as the base for `--prefix` and the merge "current" side). A missing
/// index yields an empty map.
fn current_index_stage0(git_dir: &Path, format: ObjectFormat) -> Result<LeafMap> {
    let mut out = BTreeMap::new();
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for entry in index.entries {
            if entry_stage(&entry) == 0 {
                out.insert(entry.path.into_bytes(), (entry.mode, entry.oid));
            }
        }
    }
    Ok(out)
}

/// git's `ce_stat_data` for a parsed on-disk index entry: the cached `lstat`
/// fields the merge must carry forward so a kept entry stays `diff-files`-clean.
fn stat_info_from_index_entry(entry: &IndexEntry) -> sley_unpack_trees::StatInfo {
    sley_unpack_trees::StatInfo {
        ctime_seconds: entry.ctime_seconds,
        ctime_nanoseconds: entry.ctime_nanoseconds,
        mtime_seconds: entry.mtime_seconds,
        mtime_nanoseconds: entry.mtime_nanoseconds,
        dev: entry.dev,
        ino: entry.ino,
        uid: entry.uid,
        gid: entry.gid,
        size: entry.size,
    }
}

/// Read the current on-disk index's stage-0 entries into the engine's
/// [`sley_unpack_trees::FlatIndex`] — path → `(mode, oid, cached stat)` — so a
/// `read-tree -m` carries each kept entry's `lstat` info forward. A missing
/// index yields an empty map.
fn current_index_flat(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<sley_unpack_trees::FlatIndex> {
    let mut out = sley_unpack_trees::FlatIndex::new();
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for entry in &index.entries {
            if entry_stage(entry) == 0 {
                out.insert(
                    entry.path.as_bytes().to_vec(),
                    (
                        entry.mode,
                        entry.oid,
                        Some(stat_info_from_index_entry(entry)),
                    ),
                );
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
            out.insert(entry.path.into_bytes());
        }
    }
    Ok(out)
}

/// Which command's porcelain error strings the engine's safety checks should
/// emit, mirroring git's `setup_unpack_trees_porcelain(o, cmd)`. The merge
/// rules are identical across commands; only the *user-facing abort text*
/// differs ("...by checkout" vs "...by merge", and the trailing
/// "switch branches" vs "merge" hint). `checkout <branch>` / `switch` /
/// `checkout --detach` all use [`UnpackPorcelain::Checkout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnpackPorcelain {
    /// `read-tree -m`'s historic per-path messages (`Entry '...' not uptodate.
    /// Cannot merge.` / `Untracked working tree file '...' would be overwritten
    /// by merge.`). The default for the plumbing `read-tree` consumer.
    ReadTree,
    /// `git checkout` / `git switch` porcelain: the collected-path abort block
    /// (`Your local changes to the following files would be overwritten by
    /// checkout:\n\t<path>\nPlease commit your changes or stash them before you
    /// switch branches.\nAborting`).
    Checkout,
}

/// The working-tree side of a `-m` merge, supplying the `sley-unpack-trees`
/// engine with read-tree's I/O: how to tell whether a path is up to date
/// (hashing the worktree blob), whether materializing/removing a path would
/// clobber an untracked file, and how to write/remove worktree files when `-u`
/// applies the result.
struct ReadTreeWorktree<'a> {
    worktree_root: PathBuf,
    git_dir: PathBuf,
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    /// Every path present in the *pre-merge* index (any stage). A merged-result
    /// path not in this set is a fresh addition, so writing it must not clobber
    /// an untracked working-tree file.
    original_paths: BTreeSet<Vec<u8>>,
    /// Typed `.gitmodules` of the superproject worktree, parsed once. `None`
    /// when there is no `.gitmodules` (then no path is a submodule and the
    /// move-head hook is a no-op). This is git's `submodule_from_ce` source.
    submodules: Option<sley_submodule::SubmoduleConfigSet>,
    /// The superproject's `.git/config`, parsed once, for the
    /// `is_submodule_active` resolution (`submodule.<name>.active` /
    /// `submodule.active` / `submodule.<name>.url`).
    repo_config: GitConfig,
    /// Which command's abort text the safety checks emit (git's
    /// `setup_unpack_trees_porcelain`). `read-tree` keeps its historic
    /// per-path messages; `checkout`/`switch` use the collected-path block.
    porcelain: UnpackPorcelain,
    /// Whether tree application should run the submodule move-head mutation path
    /// rather than only creating/removing the gitlink directory placeholder.
    recurse_submodules: bool,
    /// Force checkout/reset mode: tracked worktree modifications may be
    /// overwritten, so `verify_uptodate` must not reject them.
    force_overwrite_tracked: bool,
}

impl sley_unpack_trees::WorktreeProbe for ReadTreeWorktree<'_> {
    fn verify_uptodate(&self, path: &[u8], ce: &sley_unpack_trees::CacheEntry) -> Result<()> {
        if self.force_overwrite_tracked {
            return Ok(());
        }
        // git's `verify_uptodate_1` short-circuits a gitlink (submodule):
        // `if (S_ISGITLINK(ce->ce_mode)) return 0;` — a submodule is never
        // "dirty" via a worktree blob hash (its working tree is a *directory*,
        // not a blob), so the engine must not try to `fs::read` it. The
        // submodule's own dirtiness is handled separately by
        // `check_submodule_move_head`.
        if sley_index::is_gitlink(ce.mode) {
            return Ok(());
        }
        // The engine hands us the *current index* entry for the path; reuse the
        // existing hash-the-worktree-blob comparison (a missing tracked file is
        // treated as up to date, matching git's re-materialization allowance).
        verify_uptodate_path(
            &self.worktree_root,
            &self.git_dir,
            self.format,
            &self.repo_config,
            path,
            Some(&(ce.mode, ce.oid)),
            self.porcelain,
        )
    }

    fn verify_absent_overwrite(
        &self,
        path: &[u8],
        merge: &sley_unpack_trees::CacheEntry,
        reset: sley_unpack_trees::ResetType,
    ) -> Result<()> {
        // git's `verify_absent(ERROR_WOULD_LOSE_UNTRACKED_OVERWRITTEN)`: a brand
        // new path must not write over an untracked file. A path already in the
        // pre-merge index is tracked, so re-materializing it is fine.
        if self.original_paths.contains(path) {
            return Ok(());
        }
        // `--reset` (OverwriteUntracked) authorizes clobbering anything in the
        // way (git's `o->reset == UNPACK_RESET_OVERWRITE_UNTRACKED` early return).
        if matches!(reset, sley_unpack_trees::ResetType::OverwriteUntracked) {
            if original_cwd_relative_to(&self.worktree_root).as_deref() == Some(path) {
                return refuse_remove_current_working_directory(path);
            }
            return Ok(());
        }
        let Some(file_path) = safe_worktree_path(&self.worktree_root, path) else {
            return Ok(());
        };
        let Ok(metadata) = fs::symlink_metadata(&file_path) else {
            return Ok(());
        };
        if sley_worktree::path_matches_standard_ignore(
            &self.worktree_root,
            path,
            metadata.is_dir(),
        )? {
            return Ok(());
        }
        // git's `check_ok_to_remove`: a directory in the way (the D/F dir→file
        // transition) is checked by `verify_clean_subdirectory` — it is only OK
        // to replace when nothing untracked-and-not-ignored lives under it, and
        // every tracked file under it is itself up to date. The writer then
        // removes the subtree.
        if metadata.is_dir() {
            // git's `verify_clean_subdirectory` S_ISGITLINK arm: when the entry
            // being extracted is a gitlink, the directory in the way IS the
            // submodule's working tree — its contents belong to the submodule,
            // not untracked superproject files to be lost. Resolve the
            // submodule's checked-out HEAD: if it already equals the target
            // gitlink oid there is nothing to update (clean); otherwise defer to
            // `verify_clean_submodule` → `check_submodule_move_head`, which is a
            // no-op for a path that is not a registered submodule and a
            // would-lose guard for a populated/dirty one.
            if sley_index::is_gitlink(merge.mode) {
                let sub_head = sley_diff_merge::gitlink_head_oid(&file_path, self.format);
                if sub_head == Some(merge.oid) {
                    return Ok(());
                }
                return self.check_submodule_move_head(path, sub_head.as_ref(), &merge.oid, reset);
            }
            return self.verify_clean_subdirectory(path, &file_path);
        }
        match self.porcelain {
            UnpackPorcelain::ReadTree => {
                let display = String::from_utf8_lossy(path);
                eprintln!(
                    "error: Untracked working tree file '{display}' would be overwritten by merge."
                );
            }
            UnpackPorcelain::Checkout => {
                eprintln!(
                    "error: The following untracked working tree files would be overwritten by checkout:"
                );
                eprintln!("\t{}", String::from_utf8_lossy(path));
                eprintln!("Please move or remove them before you switch branches.");
                eprintln!("Aborting");
            }
        }
        Err(GitError::Exit(128))
    }

    fn verify_absent_remove(
        &self,
        _path: &[u8],
        _reset: sley_unpack_trees::ResetType,
    ) -> Result<()> {
        // read-tree's pre-engine path never rejected a removal on an untracked
        // file in the way (deletions only ran verify_uptodate on the tracked
        // copy); preserve that behaviour exactly to hold parity. The full
        // ERROR_WOULD_LOSE_UNTRACKED_REMOVED check is a checkout/merge concern.
        // TODO(unpack-trees): wire the untracked-would-be-lost-on-removal check
        // when the checkout pilot needs it.
        Ok(())
    }

    fn check_submodule_move_head(
        &self,
        path: &[u8],
        old_oid: Option<&ObjectId>,
        new_oid: &ObjectId,
        reset: sley_unpack_trees::ResetType,
    ) -> Result<()> {
        // git's `check_submodule_move_head`: short-circuit Ok unless `path` is a
        // real submodule (`submodule_from_ce`). The engine already filtered to
        // gitlink-mode entries; here we resolve the rest of git's guard via the
        // typed `.gitmodules` and the submodule's on-disk state, then defer the
        // verdict to the shared `sley_submodule` decision engine.
        let Ok(path_str) = std::str::from_utf8(path) else {
            // A non-UTF-8 path can't match a `.gitmodules` binding (whose values
            // are UTF-8); treat as "not a submodule" → Ok, matching the
            // `submodule_from_ce == NULL` short-circuit.
            return Ok(());
        };
        let submodule = self
            .submodules
            .as_ref()
            .and_then(|set| set.from_path(path_str));
        let Some(submodule) = submodule else {
            // Not a `.gitmodules`-bound path: `submodule_from_ce(ce) == NULL`.
            return Ok(());
        };

        let sub_root = self.worktree_root.join(path_str);
        let ctx = sley_submodule::MoveHeadContext {
            // `is_submodule_active(the_repository, path)`.
            active: is_submodule_active(&self.repo_config, &submodule.name, path_str),
            // `is_submodule_populated_gently(path)` — `<path>/.git` resolves.
            populated: submodule_is_populated(&sub_root),
            // `submodule_has_dirty_index(sub)` — staged-but-uncommitted work.
            has_dirty_index: submodule_has_dirty_index(&sub_root),
        };

        // git stores the move endpoints as hex strings (`oid_to_hex`), and the
        // decision only uses `old_head.is_some()` (the dirty-index gate keys on
        // whether an old head exists); pass the hex through faithfully.
        let old_hex = old_oid.map(|o| o.to_string());
        let new_hex = new_oid.to_string();
        let reset_is_force = matches!(reset, sley_unpack_trees::ResetType::OverwriteUntracked);

        let verdict = sley_submodule::check_submodule_move_head(
            true, // already established this path IS a submodule
            &ctx,
            old_hex.as_deref(),
            Some(&new_hex),
            reset_is_force,
        );
        move_head_verdict_to_result(verdict, path_str)
    }
}

impl ReadTreeWorktree<'_> {
    /// git's `verify_clean_subdirectory`: a directory occupies `path` where the
    /// merge wants to write a file (the D/F dir→file transition). It is safe to
    /// replace only when the directory holds nothing we would lose — concretely,
    /// no **untracked** file (one absent from the pre-merge index). Every file
    /// under it that *is* tracked is already accounted for by the merge result
    /// (it will be removed or rewritten), so it does not block the replacement.
    ///
    /// On a clean subdirectory the writer's `remove_subtree` clears it before the
    /// file is written; on an unclean one this rejects with git's
    /// `ERROR_NOT_UPTODATE_DIR` exit so no untracked work is silently destroyed.
    fn verify_clean_subdirectory(&self, dir_git_path: &[u8], dir_fs_path: &Path) -> Result<()> {
        if original_cwd_relative_to(&self.worktree_root).as_deref() == Some(dir_git_path) {
            return refuse_remove_current_working_directory(dir_git_path);
        }
        let mut stack = vec![(dir_fs_path.to_path_buf(), dir_git_path.to_vec())];
        while let Some((fs_dir, git_dir)) = stack.pop() {
            let read = match fs::read_dir(&fs_dir) {
                Ok(read) => read,
                // A vanished directory is, trivially, clean.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };
            for entry in read {
                let entry = entry?;
                let name = entry.file_name();
                let mut child_git = git_dir.clone();
                child_git.push(b'/');
                child_git.extend_from_slice(name.as_encoded_bytes());
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    stack.push((entry.path(), child_git));
                    continue;
                }
                // A tracked path (present in the pre-merge index at any stage) is
                // owned by the merge; an untracked one would be lost → reject.
                if !self.original_paths.contains(&child_git) {
                    let display = String::from_utf8_lossy(dir_git_path);
                    eprintln!("error: Updating '{display}' would lose untracked files in it");
                    return Err(GitError::Exit(128));
                }
            }
        }
        Ok(())
    }
}

/// Map a [`sley_submodule::MoveHeadVerdict`] to read-tree's exit semantics:
/// `Ok` → proceed, `WouldLose` → git's `ERROR_WOULD_LOSE_SUBMODULE`
/// (`Cannot update submodule:\n%s`, naming the path) with exit 128.
fn move_head_verdict_to_result(
    verdict: sley_submodule::MoveHeadVerdict,
    path_str: &str,
) -> Result<()> {
    match verdict {
        sley_submodule::MoveHeadVerdict::Ok => Ok(()),
        sley_submodule::MoveHeadVerdict::WouldLose => {
            eprintln!("error: Cannot update submodule:\n{path_str}");
            Err(GitError::Exit(128))
        }
    }
}

impl sley_unpack_trees::WorktreeWriter for ReadTreeWorktree<'_> {
    fn write_blob(
        &mut self,
        path: &[u8],
        mode: u32,
        oid: &ObjectId,
    ) -> Result<Option<sley_unpack_trees::StatInfo>> {
        write_tree_entry_to_worktree(
            &self.worktree_root,
            &self.git_dir,
            self.format,
            self.db,
            &self.repo_config,
            None,
            path,
            mode,
            oid,
            self.recurse_submodules,
        )
    }

    fn remove_path(&mut self, path: &[u8]) -> Result<()> {
        if self.recurse_submodules && self.path_is_configured_submodule(path) {
            return remove_submodule_worktree(&self.worktree_root, &self.git_dir, path);
        }
        remove_worktree_path(&self.worktree_root, path)
    }
}

impl ReadTreeWorktree<'_> {
    fn path_is_configured_submodule(&self, path: &[u8]) -> bool {
        let Ok(path) = std::str::from_utf8(path) else {
            return false;
        };
        self.submodules
            .as_ref()
            .and_then(|set| set.from_path(path))
            .is_some()
    }
}

/// Run git's `merge_working_tree` two-way checkout (`builtin/checkout.c`)
/// through the shared [`sley_unpack_trees`] engine: switch the index + working
/// tree from `old_tree` (the HEAD being left) to `new_tree` (the branch/commit
/// being checked out), carrying forward local modifications where the merge is
/// safe and aborting with git's exact "would be overwritten by checkout"
/// message when an unsafe overwrite is detected.
///
/// This is the single two-way path behind `git checkout <branch>`,
/// `git switch`, and `git checkout --detach`: it replaces the bespoke
/// path-by-path two-way merge that previously lived in `workspace.rs` with the
/// real `twoway_merge` primitive, so the whole checkout class inherits git's
/// `verify_uptodate` / `verify_absent` / staged-deletion semantics.
///
/// Mirrors `init_topts(..., old_branch_info->commit)`: `merge = update = 1`,
/// `fn = twoway_merge`, `initial_checkout = is_index_unborn`, `reset = NONE`,
/// `trees = [old_HEAD_tree, new_tree]`. The resulting index is written to disk
/// (`write_paired_entries`) and the worktree is updated in place via
/// [`sley_unpack_trees::check_updates`].
///
/// `old_tree`/`new_tree` are the *tree* OIDs (already peeled from their
/// commits). `old_tree` is `None` for an unborn HEAD (a fresh `checkout -b`
/// from an empty repo), in which case the engine sees an empty `oldtree` side.
///
/// `porcelain` selects the abort wording (git's
/// `setup_unpack_trees_porcelain`): `checkout`/`switch` use
/// [`UnpackPorcelain::Checkout`]; `reset --keep`, which runs the identical
/// `twoway_merge` from `reset.c`, uses [`UnpackPorcelain::ReadTree`] (the
/// per-path `Entry '...' not uptodate. Cannot merge.` message its test asserts).
pub(crate) fn checkout_two_way_engine(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    old_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
    porcelain: UnpackPorcelain,
    recurse_submodules: bool,
    overwrite_untracked: bool,
) -> Result<()> {
    use sley_unpack_trees::{MergeFn, UnpackTreesOptions, check_updates, unpack_trees};

    let index = current_index_flat(git_dir, format)?;

    // git's `merge_working_tree`: `trees[0]` = the tree of the HEAD being left
    // (empty when HEAD is unborn), `trees[1]` = the tree being checked out.
    let old_leaves = match old_tree {
        Some(oid) => sley_diff_merge::flatten_tree(db, format, oid)?,
        None => sley_unpack_trees::FlatTree::new(),
    };
    let new_leaves = sley_diff_merge::flatten_tree(db, format, new_tree)?;
    let trees = vec![old_leaves, new_leaves];

    let mut opts = UnpackTreesOptions::new(format);
    opts.merge = true;
    opts.update = true;
    // `init_topts`: `o.initial_checkout = is_index_unborn(...)`. An unborn (empty)
    // index has no staged deletion to honour, so twoway_merge takes a path from
    // the new tree rather than its "deletion was staged" arm dropping it.
    opts.initial_checkout = index.is_empty();
    opts.index_only = false;
    if overwrite_untracked {
        opts.reset = sley_unpack_trees::ResetType::OverwriteUntracked;
    }

    let mut wt = ReadTreeWorktree {
        submodules: load_superproject_submodules(worktree_root),
        repo_config: read_repo_config(git_dir).unwrap_or_default(),
        worktree_root: worktree_root.to_path_buf(),
        git_dir: git_dir.to_path_buf(),
        db,
        format,
        original_paths: original_index_paths(git_dir, format)?,
        porcelain,
        recurse_submodules,
        force_overwrite_tracked: overwrite_untracked,
    };

    // git's `merge_working_tree` runs the merge to *populate the result* with
    // every up-to-date / clobber rejection collected first, then applies the
    // worktree side only if nothing was rejected. `unpack_trees` here aborts on
    // the first rejection (before `check_updates` touches the worktree), so a
    // failed checkout leaves the working tree exactly as it was — matching git's
    // "Aborting" guarantee.
    let mut result = unpack_trees(&index, &trees, MergeFn::TwoWay, &opts, &wt)?;
    refuse_if_unpack_result_removes_current_directory(worktree_root, &result)?;
    check_updates(&mut result, &opts, &mut wt)?;

    // Serialize the merged index. check_updates folded the post-write `lstat`
    // back into each freshly-written entry, so the stat info is accurate.
    let pairs: Vec<(Vec<u8>, StagedEntry)> = result
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
                },
            )
        })
        .collect();
    write_paired_entries(git_dir, format, pairs)
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
    let mut entries = read_tree_overlay(&db, format, &config, &[tree])?;
    reset_worktree_to_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        &config,
        None,
        &mut entries,
        recurse_submodules,
    )?;
    write_paired_entries(git_dir, format, entries)
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
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    tree_oids: &[ObjectId],
    update_worktree: bool,
    index_only: bool,
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

    let index = current_index_flat(git_dir, format)?;
    let path_rules = ReadTreePathRules::from_config(config);
    let trees: Vec<sley_unpack_trees::FlatTree> = tree_oids
        .iter()
        .map(|oid| {
            let tree = sley_diff_merge::flatten_tree(db, format, oid)?;
            for (path, (mode, _)) in &tree {
                verify_read_tree_path(path, *mode, path_rules)?;
            }
            Ok(tree)
        })
        .collect::<Result<_>>()?;

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

    let worktree_root = if index_only {
        worktree_root_for_git_dir(git_dir).unwrap_or_else(|_| git_dir.to_path_buf())
    } else {
        worktree_root_for_git_dir(git_dir)?
    };
    let mut wt = ReadTreeWorktree {
        submodules: load_superproject_submodules(&worktree_root),
        repo_config: read_repo_config(git_dir).unwrap_or_default(),
        worktree_root,
        git_dir: git_dir.to_path_buf(),
        db,
        format,
        original_paths: original_index_paths(git_dir, format)?,
        porcelain: UnpackPorcelain::ReadTree,
        recurse_submodules,
        force_overwrite_tracked: false,
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
                },
            )
        })
        .collect())
}

/// Parse the superproject's `.gitmodules` into the typed config set (git's
/// `submodule_from_path` source). `None` when there is no `.gitmodules`, in
/// which case no path is a submodule and the move-head hook never fires.
fn load_superproject_submodules(
    worktree_root: &Path,
) -> Option<sley_submodule::SubmoduleConfigSet> {
    let gitmodules = worktree_root.join(".gitmodules");
    let config = GitConfig::read(&gitmodules).ok()?;
    Some(sley_submodule::SubmoduleConfigSet::parse(&config))
}

/// Port of git's `is_submodule_active` (`submodule.c::is_tree_submodule_active`)
/// for the read-tree probe. The path→module mapping was already established by
/// the caller, so we only run the active-resolution chain:
///
/// 1. `submodule.<name>.active` (bool) — if set, it wins.
/// 2. `submodule.active` (multi-valued pathspec) — match `path` against it.
/// 3. fallback: `submodule.<name>.url` is set in the superproject config.
fn is_submodule_active(repo_config: &GitConfig, name: &str, path: &str) -> bool {
    // 1. submodule.<name>.active
    if let Some(active) = repo_config.get_bool("submodule", Some(name), "active") {
        return active;
    }
    // 2. submodule.active pathspec list. git matches `path` against the
    //    configured pathspecs; we support the common exact-path / prefix form
    //    (`<dir>` or `<dir>/`), which covers the `.gitmodules`-bound paths the
    //    read-tree pilot exercises. A `:(glob)`-style magic pathspec falls
    //    through to the url fallback rather than being mis-evaluated.
    let active_specs: Vec<&str> = repo_config
        .get_all("submodule", None, "active")
        .into_iter()
        .flatten()
        .collect();
    if !active_specs.is_empty() {
        return active_specs
            .iter()
            .any(|spec| pathspec_matches_submodule(spec, path));
    }
    // 3. fallback: submodule.<name>.url is configured.
    repo_config.get("submodule", Some(name), "url").is_some()
}

/// Minimal pathspec match for the `submodule.active` list: an exact path or a
/// directory-prefix match (git's `match_pathspec` for a literal pathspec).
fn pathspec_matches_submodule(spec: &str, path: &str) -> bool {
    let spec = spec.trim_end_matches('/');
    // git's `clone --recurse-submodules` records `submodule.active = .`; a `.`
    // (or empty) pathspec matches every path from the repository root, so every
    // submodule is active.
    if spec.is_empty() || spec == "." {
        return true;
    }
    path == spec || path.starts_with(&format!("{spec}/"))
}

/// git's `is_submodule_populated_gently`: does `<path>/.git` resolve to a real
/// repository? Both the embedded `.git` directory and the `.git` gitfile
/// (`gitdir: …` pointer) forms count.
fn submodule_is_populated(sub_root: &Path) -> bool {
    let dot_git = sub_root.join(".git");
    if dot_git.is_dir() {
        return true;
    }
    if dot_git.is_file() {
        // A `.git` gitfile pointing at a real gitdir → populated.
        return read_gitdir_file(&dot_git).ok().flatten().is_some();
    }
    false
}

/// git's `submodule_has_dirty_index`: does the submodule have staged but
/// uncommitted changes relative to its HEAD? git shells
/// `git diff-index --quiet --cached HEAD` in the submodule and treats a
/// non-zero exit as dirty. We do the same against the *sley* binary so the
/// answer comes from the same engine, with the submodule env scrubbed
/// (`prepare_submodule_repo_env`) so the parent repo's GIT_* vars don't leak in.
///
/// A submodule whose HEAD/index can't be read (unpopulated, no commits) is
/// treated as clean — git only reaches this with `old_head && populated`, so
/// the caller has already gated those cases.
fn submodule_has_dirty_index(sub_root: &Path) -> bool {
    if !sub_root.join(".git").exists() {
        return false;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return false,
    };
    let status = ProcessCommand::new(exe)
        .args(["diff-index", "--quiet", "--cached", "HEAD"])
        .current_dir(sub_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        // Exit 0 → clean; non-zero → dirty (git's contract).
        Ok(s) => !s.success(),
        // Could not run the check → conservatively clean (git would die, but
        // failing the whole read-tree on an introspection error is worse than
        // matching git's "no dirty work detected" for our pilot scope).
        Err(_) => false,
    }
}

/// Construct a stage-0 [`StagedEntry`] with no cached stat (the overlay /
/// `--prefix` paths build the index purely from tree contents, so no worktree
/// stat is available; git's `read-tree` without `-m` writes a zeroed stat too).
fn stage0(mode: u32, oid: ObjectId) -> StagedEntry {
    StagedEntry {
        mode,
        oid,
        stage: 0,
        stat: None,
    }
}

/// Abort with git's "not uptodate" error when the working-tree copy of `path`
/// disagrees with the index entry the merge is about to replace/remove.
///
/// `expected` is the index content the merge assumes (the stage-0 entry, or
/// `None` if the path is currently untracked). The worktree file must hash to
/// the same blob for the operation to be safe; a missing tracked file is
/// considered up to date (git permits re-materializing it).
///
/// This is the I/O behind [`ReadTreeWorktree`]'s `verify_uptodate` probe (git's
/// `verify_uptodate_1`).
fn verify_uptodate_path(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    path: &[u8],
    expected: Option<&(u32, ObjectId)>,
    porcelain: UnpackPorcelain,
) -> Result<()> {
    let Some((_mode, expected_oid)) = expected else {
        // Untracked path: nothing in the index to be out of date with.
        return Ok(());
    };
    let Some(file_path) = safe_worktree_path(worktree_root, path) else {
        return Ok(());
    };
    let body = match fs::read(&file_path) {
        Ok(body) => body,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let body = sley_worktree::apply_clean_filter(worktree_root, git_dir, config, path, &body)?;
    let actual = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    if &actual != expected_oid {
        match porcelain {
            UnpackPorcelain::ReadTree => {
                let display = String::from_utf8_lossy(path);
                eprintln!("error: Entry '{display}' not uptodate. Cannot merge.");
            }
            UnpackPorcelain::Checkout => {
                // git's ERROR_NOT_UPTODATE_FILE under the "checkout" porcelain:
                // the collected-path "local changes would be overwritten" block.
                eprintln!(
                    "error: Your local changes to the following files would be overwritten by checkout:"
                );
                eprintln!("\t{}", String::from_utf8_lossy(path));
                eprintln!("Please commit your changes or stash them before you switch branches.");
                eprintln!("Aborting");
            }
        }
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
    let skip_worktree_paths = read_skip_worktree_paths(git_dir, format)?;
    let mut index_entries = Vec::with_capacity(pairs.len());
    for (path, entry) in pairs {
        let mut index_entry = make_index_entry(path, entry)?;
        if skip_worktree_paths.contains(index_entry.path.as_bytes()) {
            index_entry.set_skip_worktree(true);
        }
        index_entries.push(index_entry);
    }
    index_entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags & 0x3000).cmp(&(right.flags & 0x3000)))
    });
    persist_index(git_dir, format, index_entries)
}

fn read_skip_worktree_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<std::collections::BTreeSet<Vec<u8>>> {
    Ok(sley_worktree::read_repository_index(git_dir, format)?
        .map(|index| {
            index
                .entries
                .into_iter()
                .filter(|entry| entry.is_skip_worktree())
                .map(|entry| entry.path.into_bytes())
                .collect()
        })
        .unwrap_or_default())
}

/// Convert a `(path, StagedEntry)` into a writable [`IndexEntry`], encoding the
/// stage into bits 12-13 of `flags` and the path length into the low 12 bits.
///
/// When the entry carries cached stat info (a kept entry, or one refreshed by
/// the `-u` apply), it is written into the entry's `ce_stat_data` fields so a
/// follow-up `diff-files` reports the correct clean/dirty verdict; otherwise
/// the stat fields stay zeroed (git's all-zero "needs refresh" state).
fn make_index_entry(path: Vec<u8>, entry: StagedEntry) -> Result<IndexEntry> {
    let name_len = path.len().min(0x0fff) as u16;
    let stage_bits = ((entry.stage as u16) & 0x3) << 12;
    let stat = entry.stat.unwrap_or_default();
    Ok(IndexEntry {
        ctime_seconds: stat.ctime_seconds,
        ctime_nanoseconds: stat.ctime_nanoseconds,
        mtime_seconds: stat.mtime_seconds,
        mtime_nanoseconds: stat.mtime_nanoseconds,
        dev: stat.dev,
        ino: stat.ino,
        mode: entry.mode,
        uid: stat.uid,
        gid: stat.gid,
        size: stat.size,
        oid: entry.oid,
        flags: name_len | stage_bits,
        flags_extended: 0,
        path: BString::from(path),
    })
}

/// Serialize `entries` into the repository index file. Stage > 0 entries set the
/// stage bits in `flags`; the index v2/v3 writer accepts those (the higher bits
/// of `flags`), so a fixed version 2 layout matches git's `ls-files --stage`.
fn persist_index(git_dir: &Path, format: ObjectFormat, entries: Vec<IndexEntry>) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    index.upgrade_version_for_flags();
    sley_worktree::refresh_cache_tree(&mut index, &db);
    sley_worktree::write_repository_index_ref(git_dir, format, &index)?;
    Ok(())
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
    let original = current_index_stage0(git_dir, format)?;
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
    refuse_if_current_working_directory_becomes_file(worktree_root, entries)?;
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

/// Write a single tree entry from the object database to `path` under
/// `worktree_root`, returning the post-write `lstat` info git records back into
/// the index entry (its `ce_stat_data`).
///
/// This is git's `checkout_entry` with `state.force = 1, refresh_cache = 1`:
///
/// * **D/F replacement.** Whatever is already at `path` — a regular file, a
///   symlink, or a whole **directory subtree** (the dir→file transition) — is
///   removed first. A gitlink (mode 160000) leaves an existing directory alone.
/// * **Leading directories.** Each missing parent component is created; a
///   non-directory in the way of a needed component is unlinked first (git's
///   `create_directories`, the file→dir transition).
/// * **Type by mode.** `0o120000` is written as a **symlink** whose target is
///   the (raw, unfiltered) blob bytes; `0o160000` (gitlink) is a directory that
///   the submodule machinery populates; everything else is a regular file with
///   the executable bit set iff the mode is `0o100755`. Regular-file content
///   goes through the smudge filter (EOL + `filter.<name>.smudge`) so the
///   materialized bytes match `git checkout`; symlink targets are opaque.
/// * **Stat-back.** The written path is `lstat`'d and its [`StatInfo`] returned
///   so [`check_updates`] folds it into the index entry — the size + mtime the
///   racy-clean machinery (`worktree_entry_is_uptodate`) keys on to keep a
///   freshly-checked-out file reported clean.
#[allow(clippy::too_many_arguments)]
fn write_blob_to_worktree(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    tree_attributes: Option<&sley_worktree::TreeAttributes>,
    path: &[u8],
    mode: u32,
    oid: &ObjectId,
) -> Result<Option<sley_unpack_trees::StatInfo>> {
    let Some(file_path) = safe_worktree_path(worktree_root, path) else {
        return Err(GitError::InvalidPath(format!(
            "invalid worktree path {}",
            String::from_utf8_lossy(path)
        )));
    };

    // A gitlink is a directory git leaves to the submodule move-head machinery;
    // it never reads an object here. Ensure the directory exists (an
    // already-populated submodule is left untouched) and record a zeroed stat,
    // exactly as git's `write_entry` S_IFGITLINK arm and `materialize_tree_entry`.
    if sley_index::is_gitlink(mode) {
        create_leading_directories(worktree_root, &file_path)?;
        if fs::symlink_metadata(&file_path).is_ok_and(|md| !md.is_dir()) {
            remove_path_in_the_way(&file_path)?;
        }
        fs::create_dir_all(&file_path)?;
        return Ok(None);
    }

    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {oid}, found {}",
            object.object_type.as_str()
        )));
    }

    // Create leading directories FIRST, unlinking a non-dir in the way of a
    // needed component (git's `create_directories`, the file→dir transition:
    // a tracked file `p` being replaced by `p/child` must first become a dir).
    // This must precede the final-path probe below, which would otherwise see
    // ENOTDIR trying to stat `p/child` under a file `p`.
    create_leading_directories(worktree_root, &file_path)?;
    // Then remove whatever currently occupies the final path: a directory
    // subtree (the D/F dir→file transition, git's `remove_subtree`) or any
    // file/symlink. `force` is always set here.
    remove_path_in_the_way(&file_path)?;

    if (mode & 0o170000) == 0o120000 {
        // Symlink: the blob bytes are the link target, opaque to clean/smudge.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let target =
                std::path::PathBuf::from(std::ffi::OsString::from_vec(object.body.clone()));
            std::os::unix::fs::symlink(&target, &file_path)?;
        }
        #[cfg(not(unix))]
        {
            // No symlink support: fall back to writing the link text as a regular
            // file, matching git's behaviour on filesystems without symlinks.
            fs::write(&file_path, &object.body)?;
        }
    } else {
        let body = match tree_attributes {
            Some(attributes) => attributes.apply_smudge_filter(config, path, &object.body)?,
            None => sley_worktree::apply_smudge_filter(
                worktree_root,
                git_dir,
                format,
                config,
                path,
                &object.body,
            )?,
        };
        fs::write(&file_path, &body)?;
        // Executable bit: 0o100755 → +x, 0o100644 → plain. git only honours the
        // user-execute bit when deciding the index mode, so set/clear it here.
        #[cfg(unix)]
        if (mode & 0o170000) == 0o100000 {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::symlink_metadata(&file_path)?.permissions();
            let mut bits = perms.mode();
            if mode & 0o111 != 0 {
                bits |= 0o111;
            } else {
                bits &= !0o111;
            }
            fs::set_permissions(&file_path, fs::Permissions::from_mode(bits))?;
        }
    }

    Ok(Some(stat_info_from_lstat(&file_path)?))
}

#[allow(clippy::too_many_arguments)]
fn write_tree_entry_to_worktree(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    tree_attributes: Option<&sley_worktree::TreeAttributes>,
    path: &[u8],
    mode: u32,
    oid: &ObjectId,
    recurse_submodules: bool,
) -> Result<Option<sley_unpack_trees::StatInfo>> {
    if recurse_submodules
        && sley_index::is_gitlink(mode)
        && gitlink_should_recurse(worktree_root, config, path)
    {
        checkout_submodule_to_commit(worktree_root, git_dir, format, path, oid)?;
        return Ok(None);
    }
    write_blob_to_worktree(
        worktree_root,
        git_dir,
        format,
        db,
        config,
        tree_attributes,
        path,
        mode,
        oid,
    )
}

fn gitlink_should_recurse(worktree_root: &Path, repo_config: &GitConfig, path: &[u8]) -> bool {
    let Ok(path_str) = std::str::from_utf8(path) else {
        return false;
    };
    let Some(submodules) = load_superproject_submodules(worktree_root) else {
        return false;
    };
    let Some(submodule) = submodules.from_path(path_str) else {
        return false;
    };
    is_submodule_active(repo_config, &submodule.name, path_str)
}

fn checkout_submodule_to_commit(
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
    if submodule_path_contains_symlink(worktree_root, path)? {
        eprintln!("fatal: refusing to checkout submodule path '{path_str}' through a symlink");
        return Err(GitError::Exit(128));
    }
    let (submodule_name, submodule_url) = submodule_name_and_url_for_path(worktree_root, &path_str)
        .unwrap_or_else(|| (path_str.to_string(), None));
    let sub_git_dir = submodule_admin_git_dir(git_dir, &submodule_name);
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
    connect_submodule_worktree(&sub_root, &sub_git_dir)?;

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
    super::remote::cmd_clone(&args)?;
    connect_submodule_worktree(sub_root, sub_git_dir)?;
    Ok(())
}

fn remove_submodule_worktree(worktree_root: &Path, git_dir: &Path, path: &[u8]) -> Result<()> {
    let Some(sub_root) = safe_worktree_path(worktree_root, path) else {
        return Ok(());
    };
    let path_str = String::from_utf8_lossy(path);
    if submodule_path_contains_symlink(worktree_root, path)? {
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

fn submodule_path_contains_symlink(worktree_root: &Path, path: &[u8]) -> Result<bool> {
    let Ok(path) = std::str::from_utf8(path) else {
        return Ok(false);
    };
    let mut current = worktree_root.to_path_buf();
    for component in Path::new(path).components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(err)
                if err.kind() == io::ErrorKind::NotFound
                    || err.kind() == io::ErrorKind::NotADirectory =>
            {
                return Ok(false);
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(false)
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

/// git's `fill_stat_cache_info`/`fill_stat_data`: `lstat` the just-written path
/// and project its fields into the engine's [`sley_unpack_trees::StatInfo`].
/// `size` is the **on-disk** byte length (so it equals `metadata.len()`, which
/// sley's `worktree_entry_is_uptodate` compares directly), and mtime is the
/// file's real mtime so the racy-clean shortcut can prove the path unchanged.
#[cfg(unix)]
fn stat_info_from_lstat(file_path: &Path) -> Result<sley_unpack_trees::StatInfo> {
    use std::os::unix::fs::MetadataExt;
    let md = fs::symlink_metadata(file_path)?;
    Ok(sley_unpack_trees::StatInfo {
        ctime_seconds: md.ctime().clamp(0, u32::MAX as i64) as u32,
        ctime_nanoseconds: (md.ctime_nsec().max(0)) as u32,
        mtime_seconds: md.mtime().clamp(0, u32::MAX as i64) as u32,
        mtime_nanoseconds: (md.mtime_nsec().max(0)) as u32,
        dev: md.dev() as u32,
        ino: md.ino() as u32,
        uid: md.uid(),
        gid: md.gid(),
        size: md.len().min(u32::MAX as u64) as u32,
    })
}

#[cfg(not(unix))]
fn stat_info_from_lstat(file_path: &Path) -> Result<sley_unpack_trees::StatInfo> {
    // The ctime/dev/ino/uid/gid stat fields are Unix-only; off Unix they are
    // zeroed (as git does on platforms without them) and only the portable
    // mtime and size are filled. The racy-clean shortcut degrades gracefully.
    let md = fs::symlink_metadata(file_path)?;
    let (mtime_seconds, mtime_nanoseconds) = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs().min(u32::MAX as u64) as u32, d.subsec_nanos()))
        .unwrap_or((0, 0));
    Ok(sley_unpack_trees::StatInfo {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds,
        mtime_nanoseconds,
        dev: 0,
        ino: 0,
        uid: 0,
        gid: 0,
        size: md.len().min(u32::MAX as u64) as u32,
    })
}

/// git's `write_entry` D/F-removal preamble: remove whatever currently occupies
/// `file_path` so a write can proceed. A directory is removed recursively (the
/// dir→file transition, git's `remove_subtree`); a file or symlink is unlinked.
/// An absent path is a no-op.
fn remove_path_in_the_way(file_path: &Path) -> Result<()> {
    match fs::symlink_metadata(file_path) {
        Ok(md) if md.is_dir() => {
            if path_is_original_cwd(file_path) {
                return refuse_remove_current_working_directory_absolute(file_path);
            }
            fs::remove_dir_all(file_path)?;
        }
        Ok(_) => {
            fs::remove_file(file_path)?;
        }
        // Nothing there (`NotFound`) or a non-directory leading component
        // (`NotADirectory`/ENOTDIR — only possible if a parent was not turned
        // into a directory, which `create_leading_directories` already does)
        // means there is nothing to clear.
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                || err.kind() == io::ErrorKind::NotADirectory => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

/// git's `create_directories`: create every leading directory of `file_path`
/// up from (and excluding) `worktree_root`, unlinking a non-directory in the way
/// of a needed component (the file→dir transition). `fs::create_dir_all` handles
/// the common all-missing case; the per-component fallback handles a regular
/// file or symlink sitting where a directory must be.
fn create_leading_directories(worktree_root: &Path, file_path: &Path) -> Result<()> {
    let Some(parent) = file_path.parent() else {
        return Ok(());
    };
    // NOTE: `fs::create_dir_all` treats an existing *file* at a needed component
    // as success (the mkdir gets EEXIST, which create_dir_all swallows without
    // checking the type), so it canNOT be trusted for the D/F file→dir
    // transition. Walk each leading component and, where a non-directory blocks
    // a needed directory, unlink it and create the directory (git's
    // `mkdir → EEXIST && force → unlink → mkdir`).
    let mut cur = worktree_root.to_path_buf();
    let rel = parent.strip_prefix(worktree_root).unwrap_or(parent);
    for component in rel.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        cur.push(name);
        match fs::symlink_metadata(&cur) {
            Ok(md) if md.is_dir() => {}
            Ok(_) => {
                if path_is_original_cwd(&cur) {
                    return refuse_remove_current_working_directory_absolute(&cur);
                }
                fs::remove_file(&cur)?;
                fs::create_dir(&cur)?;
            }
            Err(err)
                if err.kind() == io::ErrorKind::NotFound
                    || err.kind() == io::ErrorKind::NotADirectory =>
            {
                fs::create_dir(&cur)?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// git's `unlink_entry`: remove a working-tree path and prune now-empty leading
/// directories, ignoring an already-absent target. A directory occupying the
/// path (a leftover from a prior file→dir transition, or a populated gitlink
/// being removed) is removed recursively — git's `remove_or_warn` honours the
/// directory mode.
fn remove_worktree_path(worktree_root: &Path, path: &[u8]) -> Result<()> {
    let Some(file_path) = safe_worktree_path(worktree_root, path) else {
        return Ok(());
    };
    match fs::symlink_metadata(&file_path) {
        // git's `unlink_entry`: a path whose worktree copy is a *directory* is a
        // gitlink (a populated submodule) or a directory whose tracked children
        // have already been removed first (check_updates unlinks every
        // CE_WT_REMOVE entry before this one). git removes it with a *non-recursive*
        // `rmdir` and, when the directory still holds untracked content (a dirty
        // submodule), emits `warning: unable to rmdir '<path>': Directory not
        // empty` and leaves it in place — it never recursively deletes a
        // submodule's working tree.
        Ok(md) if md.is_dir() => match fs::remove_dir(&file_path) {
            Ok(()) => {}
            Err(err)
                if err.kind() == io::ErrorKind::DirectoryNotEmpty
                    || err.raw_os_error() == Some(39) =>
            {
                eprintln!(
                    "warning: unable to rmdir '{}': Directory not empty",
                    String::from_utf8_lossy(path)
                );
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        },
        Ok(_) => fs::remove_file(&file_path)?,
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
        if path == root || path_is_original_cwd(path) {
            break;
        }
        if fs::remove_dir(path).is_err() {
            break;
        }
        dir = path.parent();
    }
}

fn original_cwd_absolute() -> Option<PathBuf> {
    let cwd = sley_core::original_cwd().or_else(|| env::current_dir().ok())?;
    Some(fs::canonicalize(&cwd).unwrap_or(cwd))
}

fn original_cwd_relative_to(worktree_root: &Path) -> Option<Vec<u8>> {
    let root = fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    let cwd = original_cwd_absolute()?;
    if cwd == root {
        return None;
    }
    let rel = cwd.strip_prefix(&root).ok()?;
    Some(path_to_git_bytes_lossy(rel))
}

fn path_is_original_cwd(path: &Path) -> bool {
    let Some(cwd) = original_cwd_absolute() else {
        return false;
    };
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path == cwd
}

fn refuse_if_current_working_directory_becomes_file(
    worktree_root: &Path,
    entries: &[(Vec<u8>, StagedEntry)],
) -> Result<()> {
    let Some(cwd) = original_cwd_relative_to(worktree_root) else {
        return Ok(());
    };
    if entries.iter().any(|(path, entry)| {
        path == &cwd && !sley_index::is_gitlink(entry.mode) && (entry.mode & 0o170000) != 0o040000
    }) {
        if let Some(path) = safe_worktree_path(worktree_root, &cwd)
            && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        {
            return refuse_remove_current_working_directory(&cwd);
        }
    }
    Ok(())
}

fn refuse_if_unpack_result_removes_current_directory(
    worktree_root: &Path,
    result: &sley_unpack_trees::UnpackTreesResult,
) -> Result<()> {
    let Some(cwd) = original_cwd_relative_to(worktree_root) else {
        return Ok(());
    };
    let cwd_slash = {
        let mut value = cwd.clone();
        value.push(b'/');
        value
    };
    let writes_file_at_cwd = result.entries.iter().any(|entry| {
        entry.path == cwd
            && entry.entry.stage == 0
            && !sley_index::is_gitlink(entry.entry.mode)
            && (entry.entry.mode & 0o170000) != 0o040000
    });
    if writes_file_at_cwd
        && result
            .removed_paths
            .iter()
            .any(|removed| removed.starts_with(&cwd_slash))
    {
        return refuse_remove_current_working_directory(&cwd);
    }
    Ok(())
}

fn refuse_remove_current_working_directory(path: &[u8]) -> Result<()> {
    eprintln!(
        "error: Refusing to remove the current working directory:\n{}",
        String::from_utf8_lossy(path)
    );
    Err(GitError::Exit(128))
}

fn refuse_remove_current_working_directory_absolute(path: &Path) -> Result<()> {
    eprintln!(
        "error: Refusing to remove the current working directory:\n{}",
        path.display()
    );
    Err(GitError::Exit(128))
}

fn path_to_git_bytes_lossy(path: &Path) -> Vec<u8> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes()
}

#[cfg(test)]
mod submodule_hook_tests {
    use super::*;
    use sley_submodule::{MoveHeadContext, MoveHeadVerdict, check_submodule_move_head};

    fn config_from(text: &str) -> GitConfig {
        GitConfig::parse(text.as_bytes()).expect("valid config")
    }

    fn gitmodules_set(text: &str) -> sley_submodule::SubmoduleConfigSet {
        sley_submodule::SubmoduleConfigSet::parse(&config_from(text))
    }

    // ----- verdict → read-tree exit mapping ------------------------------

    #[test]
    fn would_lose_maps_to_exit_128() {
        let err = move_head_verdict_to_result(MoveHeadVerdict::WouldLose, "sub1")
            .expect_err("WouldLose must be an error");
        assert!(matches!(err, GitError::Exit(128)));
    }

    #[test]
    fn ok_verdict_maps_to_ok() {
        assert!(move_head_verdict_to_result(MoveHeadVerdict::Ok, "sub1").is_ok());
    }

    // ----- is_submodule_active resolution chain --------------------------

    #[test]
    fn active_explicit_true_wins() {
        let cfg = config_from("[submodule \"sub1\"]\n\tactive = true\n\turl = bogus\n");
        assert!(is_submodule_active(&cfg, "sub1", "sub1"));
    }

    #[test]
    fn active_explicit_false_wins_over_url() {
        // submodule.<name>.active = false must win even when a url is set.
        let cfg = config_from("[submodule \"sub1\"]\n\tactive = false\n\turl = bogus\n");
        assert!(!is_submodule_active(&cfg, "sub1", "sub1"));
    }

    #[test]
    fn active_falls_back_to_url() {
        // No explicit active / submodule.active list → url presence decides.
        let with_url = config_from("[submodule \"sub1\"]\n\turl = bogus\n");
        assert!(is_submodule_active(&with_url, "sub1", "sub1"));
        let without_url = config_from("[submodule \"sub1\"]\n\tbranch = main\n");
        assert!(!is_submodule_active(&without_url, "sub1", "sub1"));
    }

    #[test]
    fn active_pathspec_list_matches() {
        let cfg = config_from("[submodule]\n\tactive = sub1\n\tactive = lib/inner\n");
        assert!(is_submodule_active(&cfg, "anyname", "sub1"));
        assert!(is_submodule_active(&cfg, "anyname", "lib/inner"));
        // A path not in the active list is inactive: once `submodule.active`
        // is present it is authoritative; git does not fall through to the url
        // check.
        assert!(!is_submodule_active(&cfg, "anyname", "other"));
        // The list wins over the url fallback being absent; a non-listed path
        // with no url is inactive.
        let cfg2 = config_from("[submodule]\n\tactive = sub1\n");
        assert!(!is_submodule_active(&cfg2, "anyname", "not-listed"));
    }

    #[test]
    fn pathspec_dir_prefix_matches() {
        assert!(pathspec_matches_submodule("lib", "lib"));
        assert!(pathspec_matches_submodule("lib", "lib/inner"));
        assert!(pathspec_matches_submodule("lib/", "lib/inner"));
        assert!(!pathspec_matches_submodule("lib", "library"));
        assert!(!pathspec_matches_submodule("lib", "other"));
    }

    // ----- submodule_is_populated ----------------------------------------

    #[test]
    fn populated_detects_git_dir_and_gitfile() {
        let base =
            std::env::temp_dir().join(format!("sley-rt-pop-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&base);
        // (a) embedded `.git` directory → populated.
        let with_dir = base.join("dir_form");
        fs::create_dir_all(with_dir.join(".git")).expect("mkdir dir_form/.git");
        assert!(submodule_is_populated(&with_dir));
        // (b) `.git` gitfile pointing at a real dir → populated.
        let with_file = base.join("file_form");
        let real_gitdir = base.join("real_gitdir");
        fs::create_dir_all(&real_gitdir).expect("mkdir real_gitdir");
        fs::create_dir_all(&with_file).expect("mkdir file_form");
        fs::write(
            with_file.join(".git"),
            format!("gitdir: {}\n", real_gitdir.display()),
        )
        .expect("write .git gitfile");
        assert!(submodule_is_populated(&with_file));
        // (c) no `.git` at all → not populated.
        let empty = base.join("empty");
        fs::create_dir_all(&empty).expect("mkdir empty");
        assert!(!submodule_is_populated(&empty));
        let _ = fs::remove_dir_all(&base);
    }

    // ----- end-to-end: active+populated+dirty, non-forced → WouldLose ----

    #[test]
    fn dirty_active_populated_nonforced_would_lose_and_errors() {
        // This is the cell-47/48 shape: a submodule whose HEAD is moving (old
        // set) and whose index is dirty, not forced → ERROR_WOULD_LOSE_SUBMODULE.
        let _set = gitmodules_set("[submodule \"sub1\"]\n\tpath = sub1\n\turl = ./sub1\n");
        let ctx = MoveHeadContext {
            active: true,
            populated: true,
            has_dirty_index: true,
        };
        let verdict = check_submodule_move_head(true, &ctx, Some("oldhex"), Some("newhex"), false);
        assert_eq!(verdict, MoveHeadVerdict::WouldLose);
        assert!(matches!(
            move_head_verdict_to_result(verdict, "sub1"),
            Err(GitError::Exit(128))
        ));
    }

    #[test]
    fn forced_reset_bypasses_dirty_index() {
        // The `--reset` (force) path: a dirty submodule does NOT block.
        let ctx = MoveHeadContext {
            active: true,
            populated: true,
            has_dirty_index: true,
        };
        let verdict = check_submodule_move_head(true, &ctx, Some("oldhex"), Some("newhex"), true);
        assert_eq!(verdict, MoveHeadVerdict::Ok);
        assert!(move_head_verdict_to_result(verdict, "sub1").is_ok());
    }

    #[test]
    fn non_gitmodules_path_is_not_a_submodule() {
        // A gitlink path with no `.gitmodules` binding → from_path is None →
        // the probe short-circuits Ok (git's submodule_from_ce == NULL).
        let set = gitmodules_set("[submodule \"other\"]\n\tpath = other\n\turl = ./other\n");
        assert!(set.from_path("sub1").is_none());
    }
}
