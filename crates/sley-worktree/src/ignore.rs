//! Untracked-path discovery, `.gitignore` matching, the untracked-cache builder, and the ignore matcher.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::attributes::*;
use crate::index_io::*;
use crate::status::*;
use crate::types_admin::*;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn untracked_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<Vec<u8>>> {
    untracked_paths_with_options(
        worktree_root,
        git_dir,
        format,
        UntrackedPathOptions::default(),
    )
}

/// Pathspec filter for untracked collection. Mirrors git `ls-files` pathspec
/// semantics: literal paths, recursive directory prefixes, and fnmatch globs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedPathspecFilter {
    pub path: Vec<u8>,
    pub recursive: bool,
    pub is_glob: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UntrackedPathOptions {
    pub directory: bool,
    pub no_empty_directory: bool,
    pub preserve_ignored_directories: bool,
    pub exclude_standard: bool,
    pub ignored_only: bool,
    pub exclude_patterns: Vec<Vec<u8>>,
    pub exclude_per_directory: Vec<String>,
    pub pathspecs: Vec<UntrackedPathspecFilter>,
}

// The wildmatch engine and the single-item pathspec matcher now live in the
// shared `sley-pathspec` crate. Re-export them so existing `sley-worktree`
// callers (and the t3070 `ls-files` path) keep their public surface unchanged.
pub use sley_pathspec::{
    PathspecMatchMagic, WM_CASEFOLD, WM_PATHNAME, pathspec_is_glob, pathspec_item_matches,
    wildmatch,
};

/// Whether `path` matches an `ls-files` pathspec (literal, directory prefix, or glob).
pub fn untracked_pathspec_matches(spec: &UntrackedPathspecFilter, path: &[u8]) -> bool {
    if spec.path.is_empty() {
        return true;
    }
    let path_no_slash = path.strip_suffix(b"/").unwrap_or(path);
    if path == spec.path.as_slice() || path_no_slash == spec.path.as_slice() {
        return true;
    }
    if spec.recursive
        && let Some(rest) = path
            .strip_prefix(spec.path.as_slice())
            .and_then(|rest| rest.strip_prefix(b"/"))
        && !rest.is_empty()
    {
        return true;
    }
    if spec.is_glob {
        return untracked_wildmatch(&spec.path, path)
            || untracked_wildmatch(&spec.path, path_no_slash);
    }
    false
}

/// Whether a directory walk must descend into `parent` to satisfy active pathspecs.
pub fn untracked_pathspec_needs_descent(parent: &[u8], specs: &[UntrackedPathspecFilter]) -> bool {
    if specs.is_empty() {
        return false;
    }
    let parent_prefix = if parent.is_empty() {
        Vec::new()
    } else {
        let mut prefix = parent.to_vec();
        prefix.push(b'/');
        prefix
    };
    for spec in specs {
        if !parent.is_empty()
            && spec.path.starts_with(&parent_prefix)
            && spec.path.as_slice() != parent
        {
            return true;
        }
        if spec.is_glob && glob_pathspec_may_match_under(&spec.path, parent) {
            return true;
        }
        if spec.recursive
            && !parent.is_empty()
            && parent.starts_with(spec.path.as_slice())
            && parent != spec.path.as_slice()
        {
            return true;
        }
    }
    false
}

/// Whether some pathspec selects the directory `git_path` *as a whole* (so an
/// untracked directory can roll up to `dir/` under `--directory`), as opposed to
/// only matching something strictly below it (which forces descent). A
/// directory-prefix pathspec covering the directory, an exact directory match, or
/// a glob matching the directory's own name all count; a deeper glob such as
/// `dir/*.c` or an exact file path inside the directory does not.
pub(crate) fn untracked_pathspec_selects_directory(
    specs: &[UntrackedPathspecFilter],
    git_path: &[u8],
) -> bool {
    specs
        .iter()
        .any(|spec| untracked_pathspec_matches(spec, git_path))
}

pub(crate) fn glob_pathspec_may_match_under(pattern: &[u8], dir: &[u8]) -> bool {
    let literal_prefix = literal_prefix_before_glob(pattern);
    if literal_prefix.is_empty() {
        return true;
    }
    if dir.is_empty() {
        return true;
    }
    let mut dir_prefix = dir.to_vec();
    dir_prefix.push(b'/');
    if literal_prefix.starts_with(&dir_prefix) {
        return true;
    }
    if dir_prefix.starts_with(&literal_prefix) {
        return true;
    }
    literal_prefix
        .strip_suffix(b"/")
        .is_some_and(|prefix| prefix == dir)
}

pub(crate) fn literal_prefix_before_glob(pattern: &[u8]) -> Vec<u8> {
    let mut prefix = Vec::new();
    for &byte in pattern {
        if pathspec_is_glob(&[byte]) {
            break;
        }
        prefix.push(byte);
    }
    prefix
}

pub(crate) fn insert_untracked_directory(paths: &mut BTreeSet<Vec<u8>>, git_path: &[u8]) {
    let mut directory = git_path.to_vec();
    if directory.last() != Some(&b'/') {
        directory.push(b'/');
    }
    paths.insert(directory);
}

/// fnmatch-style glob where `*` and `?` match any byte including `/`.
pub(crate) fn untracked_wildmatch(pattern: &[u8], text: &[u8]) -> bool {
    // Untracked-walk pathspec globs match with PATHMATCH semantics (`*` crosses
    // `/`), matching git's default (non-GLOB-magic) pathspec behavior.
    wildmatch(pattern, text, 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreMatch {
    pub source: Vec<u8>,
    pub line_number: usize,
    pub pattern: Vec<u8>,
    pub ignored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeState {
    Set,
    Unset,
    Value(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeCheck {
    pub attribute: Vec<u8>,
    pub state: Option<AttributeState>,
}

pub fn untracked_paths_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: UntrackedPathOptions,
) -> Result<Vec<Vec<u8>>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let (index, stat_cache, _) = read_index_entries_with_stat_cache(git_dir, format, &db)?;
    let all_index_paths = read_all_index_paths(git_dir, format)?;
    let ignores = IgnoreMatcher::from_sources(
        worktree_root,
        options.exclude_standard,
        &options.exclude_patterns,
        &options.exclude_per_directory,
    )?;
    if options.ignored_only {
        return ignored_untracked_paths(
            worktree_root,
            git_dir,
            &index,
            &ignores,
            options.directory,
        );
    }
    if options.directory {
        let mut paths = BTreeSet::new();
        collect_untracked_directory_paths(
            worktree_root,
            git_dir,
            worktree_root,
            &index,
            &ignores,
            &options,
            &mut paths,
        )?;
        return Ok(paths.into_iter().collect());
    }
    let worktree = worktree_entries_with_stat_cache(
        worktree_root,
        git_dir,
        format,
        Some(&stat_cache),
        None,
        None,
    )?;
    Ok(ls_files_untracked_paths_from_worktree(
        &worktree,
        &index,
        &all_index_paths,
        &ignores,
    ))
}

pub fn killed_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<Vec<u8>>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let bytes = match fs::read(index_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let index = Index::parse(&bytes, format)?;
    let mut exact = BTreeMap::new();
    let mut directories = BTreeSet::new();
    for entry in index
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
    {
        let path = entry.path.as_bytes();
        exact.insert(path.to_vec(), entry.mode);
        for (idx, byte) in path.iter().enumerate() {
            if *byte == b'/' && idx > 0 {
                directories.insert(path[..idx].to_vec());
            }
        }
    }
    let mut paths = BTreeSet::new();
    collect_killed_paths(
        git_dir,
        worktree_root,
        &[],
        &exact,
        &directories,
        &mut paths,
    )?;
    Ok(paths.into_iter().collect())
}

fn collect_killed_paths(
    git_dir: &Path,
    dir: &Path,
    dir_git_path: &[u8],
    exact: &BTreeMap<Vec<u8>, u32>,
    directories: &BTreeSet<Vec<u8>>,
    paths: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    if is_same_path(dir, git_dir) {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if is_dot_git_entry(&path) || is_same_path(&path, git_dir) {
            continue;
        }
        let git_path = git_path_append_component(dir_git_path, &entry.file_name());
        let file_type = entry.file_type()?;
        if let Some(mode) = exact.get(&git_path) {
            if file_type.is_dir() && !sley_index::is_gitlink(*mode) && *mode != SPARSE_DIR_MODE {
                collect_killed_leaves(git_dir, &path, &git_path, paths)?;
            }
            continue;
        }
        if directories.contains(&git_path) {
            if file_type.is_dir() {
                collect_killed_paths(git_dir, &path, &git_path, exact, directories, paths)?;
            } else if file_type.is_file() || file_type.is_symlink() {
                paths.insert(git_path);
            }
        }
    }
    Ok(())
}

fn collect_killed_leaves(
    git_dir: &Path,
    dir: &Path,
    dir_git_path: &[u8],
    paths: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    if is_same_path(dir, git_dir) {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if is_dot_git_entry(&path) || is_same_path(&path, git_dir) {
            continue;
        }
        let git_path = git_path_append_component(dir_git_path, &entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_killed_leaves(git_dir, &path, &git_path, paths)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            paths.insert(git_path);
        }
    }
    Ok(())
}

/// Untracked paths for `ls-files --others` (without `--directory`): every
/// untracked file is listed individually, except embedded-repository boundaries
/// which are emitted as `dir/` to match git's non-submodule `.git` handling.
pub(crate) fn ls_files_untracked_paths_from_worktree(
    worktree: &BTreeMap<Vec<u8>, TrackedEntry>,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    all_index_paths: &BTreeSet<Vec<u8>>,
    ignores: &IgnoreMatcher,
) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for (path, entry) in worktree {
        if index.contains_key(path)
            || all_index_paths.contains(path)
            || ignores.is_ignored(path, false)
        {
            continue;
        }
        if entry.mode == 0o040000 && entry.oid.is_null() {
            insert_untracked_directory(&mut paths, path);
            continue;
        }
        paths.insert(path.clone());
    }
    paths.into_iter().collect()
}

pub fn path_matches_standard_ignore(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    is_dir: bool,
) -> Result<bool> {
    path_matches_ignore(worktree_root, path, is_dir, true, &[])
}

pub fn standard_ignore_match(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    is_dir: bool,
) -> Result<Option<IgnoreMatch>> {
    let ignores = IgnoreMatcher::from_worktree_root(worktree_root.as_ref())?;
    Ok(ignores.match_for(path, is_dir).map(IgnorePattern::to_match))
}

pub fn standard_attributes_for_path(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    requested: &[Vec<u8>],
    all: bool,
) -> Result<Vec<AttributeCheck>> {
    let matcher = AttributeMatcher::from_worktree_root(worktree_root.as_ref())?;
    Ok(matcher.attributes_for_path(path, requested, all))
}

/// A reusable matcher for standard worktree attributes (global or
/// `core.attributesFile`, every in-tree `.gitattributes`, and
/// `$GIT_DIR/info/attributes`).
///
/// This is behaviourally identical to [`standard_attributes_for_path`] except
/// the attribute sources are read once and reused for each path.
pub struct StandardAttributeMatcher {
    matcher: AttributeMatcher,
}

impl StandardAttributeMatcher {
    pub fn from_worktree_root(worktree_root: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            matcher: AttributeMatcher::from_worktree_root(worktree_root.as_ref())?,
        })
    }

    pub fn attributes_for_path(
        &self,
        path: &[u8],
        requested: &[Vec<u8>],
        all: bool,
    ) -> Vec<AttributeCheck> {
        self.matcher.attributes_for_path(path, requested, all)
    }
}

pub fn standard_attributes_for_path_in_repo(
    attr_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    path: &[u8],
    requested: &[Vec<u8>],
    all: bool,
    include_worktree_attributes: bool,
    ignore_case: bool,
) -> Result<Vec<AttributeCheck>> {
    let attr_root = attr_root.as_ref();
    let git_dir = git_dir.as_ref();
    let mut matcher = AttributeMatcher::default();
    matcher.configure_case_sensitivity(git_dir);
    matcher.ignore_case = ignore_case;
    if !matcher.read_configured_attributes(attr_root, git_dir) {
        matcher.read_default_global_attributes();
    }
    if include_worktree_attributes {
        collect_attribute_patterns(attr_root, attr_root, &mut matcher)?;
    }
    read_attribute_patterns(
        git_dir.join("info").join("attributes"),
        &mut matcher,
        &[],
        b"info/attributes",
        false,
    );
    Ok(matcher.attributes_for_path(path, requested, all))
}

pub fn standard_attributes_for_path_from_tree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    path: &[u8],
    requested: &[Vec<u8>],
    all: bool,
) -> Result<Vec<AttributeCheck>> {
    let mut matcher = AttributeMatcher::default();
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    matcher.configure_case_sensitivity(git_dir);
    if !matcher.read_configured_attributes(worktree_root, git_dir) {
        matcher.read_default_global_attributes();
    }
    collect_attribute_patterns_from_tree(db, format, tree_oid, Vec::new(), &mut matcher)?;
    read_attribute_patterns(
        git_dir.join("info").join("attributes"),
        &mut matcher,
        &[],
        b"info/attributes",
        false,
    );
    Ok(matcher.attributes_for_path(path, requested, all))
}

pub fn standard_attributes_for_path_from_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    path: &[u8],
    requested: &[Vec<u8>],
    all: bool,
) -> Result<Vec<AttributeCheck>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let mut matcher = AttributeMatcher::default();
    matcher.configure_case_sensitivity(git_dir);
    if !matcher.read_configured_attributes(worktree_root, git_dir) {
        matcher.read_default_global_attributes();
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    collect_attribute_patterns_from_index(git_dir, format, &db, &mut matcher)?;
    read_attribute_patterns(
        git_dir.join("info").join("attributes"),
        &mut matcher,
        &[],
        b"info/attributes",
        false,
    );
    Ok(matcher.attributes_for_path(path, requested, all))
}

pub fn path_matches_ignore(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    is_dir: bool,
    exclude_standard: bool,
    exclude_patterns: &[Vec<u8>],
) -> Result<bool> {
    path_matches_ignore_with_per_directory(
        worktree_root,
        path,
        is_dir,
        exclude_standard,
        exclude_patterns,
        &[],
    )
}

pub fn path_matches_ignore_with_per_directory(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    is_dir: bool,
    exclude_standard: bool,
    exclude_patterns: &[Vec<u8>],
    exclude_per_directory: &[String],
) -> Result<bool> {
    let ignores = IgnoreMatcher::from_sources(
        worktree_root.as_ref(),
        exclude_standard,
        exclude_patterns,
        exclude_per_directory,
    )?;
    Ok(ignores.is_ignored(path, is_dir))
}

pub fn ignored_index_entries<'a>(
    worktree_root: impl AsRef<Path>,
    entries: &'a [IndexEntry],
    exclude_standard: bool,
    exclude_patterns: &[Vec<u8>],
    exclude_per_directory: &[String],
) -> Result<Vec<&'a IndexEntry>> {
    let ignores = IgnoreMatcher::from_sources(
        worktree_root.as_ref(),
        exclude_standard,
        exclude_patterns,
        exclude_per_directory,
    )?;
    Ok(entries
        .iter()
        .filter(|entry| ignores.is_ignored(entry.path.as_bytes(), false))
        .collect())
}

pub(crate) fn collect_untracked_directory_paths(
    root: &Path,
    git_dir: &Path,
    dir: &Path,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
    options: &UntrackedPathOptions,
    paths: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    if is_same_path(dir, git_dir) {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_embedded_git_internals(root, &path) {
            continue;
        }
        if is_same_path(&path, git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let relative = path.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if index
            .get(&git_path)
            .is_some_and(|entry| sley_index::is_gitlink(entry.mode))
        {
            continue;
        }
        if ignores.is_ignored(&git_path, metadata.is_dir()) {
            continue;
        }
        if metadata.is_dir() {
            if is_nested_repository_boundary(&path, git_dir) {
                insert_untracked_directory(paths, &git_path);
                continue;
            }
            let has_tracked_below = index_has_path_under(index, &git_path);
            let needs_descent = untracked_pathspec_needs_descent(&git_path, &options.pathspecs);
            if has_tracked_below {
                collect_untracked_directory_paths(
                    root, git_dir, &path, index, ignores, options, paths,
                )?;
            } else if active_repository_worktree_dir(&path, git_dir) {
                insert_untracked_directory(paths, &git_path);
            } else if needs_descent {
                // A pathspec reaches into this wholly-untracked directory. Git's
                // `--directory` still rolls it up to `dir/` when a pathspec selects
                // the directory *as a whole* (a directory-prefix that covers it, or
                // a glob matching its name). It descends only when a pathspec
                // targets something strictly below it that does not select the
                // directory itself (e.g. a deeper glob like `dir/*.c` or an exact
                // file path).
                if untracked_pathspec_selects_directory(&options.pathspecs, &git_path) {
                    insert_untracked_directory(paths, &git_path);
                    continue;
                }
                collect_untracked_directory_paths(
                    root, git_dir, &path, index, ignores, options, paths,
                )?;
            } else if options.preserve_ignored_directories
                && directory_has_ignored(&path, root, git_dir, ignores)?
            {
                collect_untracked_directory_paths(
                    root, git_dir, &path, index, ignores, options, paths,
                )?;
            } else if !options.no_empty_directory
                || directory_has_file(&path, root, git_dir, ignores)?
            {
                insert_untracked_directory(paths, &git_path);
            }
        } else if !index.contains_key(&git_path)
            && (metadata.is_file() || metadata.file_type().is_symlink())
            && (options.pathspecs.is_empty()
                || options
                    .pathspecs
                    .iter()
                    .any(|spec| untracked_pathspec_matches(spec, &git_path)))
        {
            // A file reached here was found by descending into its parent
            // directory, which happens only when that directory is not eligible
            // for rollup (it contains tracked content, has ignored entries `-d`
            // must preserve, or a pathspec selects something strictly below it).
            // Git's `--directory` rollup is a directory-level decision made when
            // the whole directory matches; an individually-reached file is always
            // listed individually.
            paths.insert(git_path);
        }
    }
    Ok(())
}

pub(crate) fn index_has_path_under(
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    directory: &[u8],
) -> bool {
    // The index map is sorted, so a single range query finds whether any tracked
    // path lives under `directory/` in O(log n) — scanning every key was O(n) per
    // untracked directory (quadratic over a deep untracked tree).
    let mut prefix = directory.to_vec();
    prefix.push(b'/');
    index
        .range::<[u8], _>((
            std::ops::Bound::Included(prefix.as_slice()),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .is_some_and(|(path, _)| path.starts_with(&prefix))
}

/// Derives normal-mode untracked paths (directory rollup) from the worktree map
/// produced by the single status walk, avoiding a third filesystem traversal.
pub(crate) fn normal_untracked_paths_from_worktree(
    worktree: &BTreeMap<Vec<u8>, TrackedEntry>,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for (path, entry) in worktree {
        if index.contains_key(path) || path_or_parent_is_ignored(ignores, path, false) {
            continue;
        }
        if entry.mode == 0o040000 && entry.oid.is_null() {
            insert_untracked_directory(&mut paths, path);
            continue;
        }
        paths.insert(untracked_normal_rollup_path(path, index, ignores));
    }
    paths.into_iter().collect()
}

pub(crate) fn path_or_parent_is_ignored(
    ignores: &IgnoreMatcher,
    path: &[u8],
    is_dir: bool,
) -> bool {
    if ignores.is_ignored(path, is_dir) {
        return true;
    }
    for (index, byte) in path.iter().enumerate() {
        if *byte == b'/' && index > 0 && ignores.is_ignored(&path[..index], true) {
            return true;
        }
    }
    false
}

pub(crate) fn status_untracked_paths_from_index(
    root: &Path,
    git_dir: &Path,
    index: &Index,
    stat_cache: &IndexStatCache,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&mut StatusProfileCounters>,
) -> Result<Vec<Vec<u8>>> {
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    let tracked_dirs = stage0_tracked_directories(index);
    let tracked = IndexStatusLookup {
        stat_cache,
        tracked_dirs: &tracked_dirs,
    };
    let mut context = StatusUntrackedWalk {
        git_dir,
        tracked: &tracked,
        ignores,
        untracked_mode,
        profile,
    };
    collect_status_untracked_paths(&mut context, root, &[], &mut paths)?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(crate) fn status_untracked_paths_from_borrowed_index(
    root: &Path,
    git_dir: &Path,
    index: &BorrowedIndex<'_>,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&mut StatusProfileCounters>,
) -> Result<Vec<Vec<u8>>> {
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(Vec::new());
    }
    let (mut paths, local_profile) = collect_status_untracked_paths_from_borrowed_index_parallel(
        root,
        git_dir,
        index,
        ignores.clone(),
        untracked_mode,
    )?;
    if let Some(profile) = profile {
        profile.merge_untracked(local_profile);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(crate) fn stream_status_untracked_paths_from_borrowed_index<F>(
    root: &Path,
    git_dir: &Path,
    index: &BorrowedIndex<'_>,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&mut StatusProfileCounters>,
    mut emit: F,
) -> Result<()>
where
    F: for<'a> FnMut(&'a [u8]) -> Result<StreamControl>,
{
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(());
    }
    let tracked = BorrowedIndexLookup::new(&index.entries);
    let mut context = StatusUntrackedWalk {
        git_dir,
        tracked: &tracked,
        ignores,
        untracked_mode,
        profile,
    };
    stream_status_untracked_paths(&mut context, root, &[], &mut emit).map(|_| ())
}

pub(crate) fn status_untracked_count_from_borrowed_index(
    root: &Path,
    git_dir: &Path,
    index: &BorrowedIndex<'_>,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&mut StatusProfileCounters>,
) -> Result<usize> {
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(0);
    }
    let (paths, local_profile) = collect_status_untracked_paths_from_borrowed_index_parallel(
        root,
        git_dir,
        index,
        ignores.clone(),
        untracked_mode,
    )?;
    if let Some(profile) = profile {
        profile.merge_untracked(local_profile);
    }
    Ok(paths.len())
}

pub(crate) fn collect_status_untracked_paths_from_borrowed_index_parallel(
    root: &Path,
    git_dir: &Path,
    index: &BorrowedIndex<'_>,
    ignores: IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
) -> Result<(Vec<Vec<u8>>, StatusProfileCounters)> {
    let executor = StatusExecutor::new();
    let mut frontier = vec![StatusUntrackedFrontierTask {
        dir: root.to_path_buf(),
        git_path: Vec::new(),
        ignores,
    }];
    let mut paths = Vec::new();
    let mut profile = StatusProfileCounters::default();

    while !frontier.is_empty() {
        let worker_count = executor.worker_count_for(frontier.len(), 1, 8);
        let output = if worker_count <= 1 {
            let tracked = BorrowedIndexLookup::new(&index.entries);
            let mut output = StatusUntrackedFrontierOutput::default();
            for mut task in frontier {
                let mut context = StatusUntrackedWalk {
                    git_dir,
                    tracked: &tracked,
                    ignores: &mut task.ignores,
                    untracked_mode,
                    profile: Some(&mut output.profile),
                };
                collect_status_untracked_frontier_dir(
                    &mut context,
                    &task.dir,
                    &task.git_path,
                    &mut output.paths,
                    &mut output.next,
                )?;
            }
            output
        } else {
            let next_task = AtomicUsize::new(0);
            std::thread::scope(|scope| -> Result<StatusUntrackedFrontierOutput> {
                let mut handles = Vec::new();
                for _ in 0..worker_count {
                    let frontier = &frontier;
                    let next_task = &next_task;
                    handles.push(executor.spawn(
                        scope,
                        "status-untracked-frontier",
                        move || -> Result<StatusUntrackedFrontierOutput> {
                            let tracked = BorrowedIndexLookup::new(&index.entries);
                            let mut output = StatusUntrackedFrontierOutput::default();
                            loop {
                                let task_idx = next_task.fetch_add(1, Ordering::Relaxed);
                                let Some(mut task) = frontier.get(task_idx).cloned() else {
                                    break;
                                };
                                let mut context = StatusUntrackedWalk {
                                    git_dir,
                                    tracked: &tracked,
                                    ignores: &mut task.ignores,
                                    untracked_mode,
                                    profile: Some(&mut output.profile),
                                };
                                collect_status_untracked_frontier_dir(
                                    &mut context,
                                    &task.dir,
                                    &task.git_path,
                                    &mut output.paths,
                                    &mut output.next,
                                )?;
                            }
                            Ok(output)
                        },
                    )?);
                }

                let mut combined = StatusUntrackedFrontierOutput::default();
                for handle in handles {
                    let mut output = handle.join()?;
                    combined.paths.append(&mut output.paths);
                    combined.next.append(&mut output.next);
                    combined.profile.merge_untracked(output.profile);
                }
                Ok(combined)
            })?
        };

        let mut output = output;
        paths.append(&mut output.paths);
        profile.merge_untracked(output.profile);
        frontier = output.next;
    }

    Ok((paths, profile))
}

pub(crate) trait StatusTrackedLookup {
    fn tracked_kind(&self, git_path: &[u8]) -> Option<StatusTrackedKind>;
    fn tracked_oid(&self, git_path: &[u8]) -> Option<ObjectId>;
    fn tracked_directory_kind(&self, git_path: &[u8]) -> Option<StatusTrackedDirectoryKind>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusTrackedKind {
    File,
    Gitlink,
    SkipWorktree,
}

impl StatusTrackedKind {
    fn from_mode_and_skip(mode: u32, skip_worktree: bool) -> Self {
        if sley_index::is_gitlink(mode) {
            Self::Gitlink
        } else if skip_worktree {
            Self::SkipWorktree
        } else {
            Self::File
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusTrackedDirectoryKind {
    ContainsTracked,
    TrackedExcluded,
}

pub(crate) struct IndexStatusLookup<'a> {
    stat_cache: &'a IndexStatCache,
    tracked_dirs: &'a HashSet<&'a [u8]>,
}

impl StatusTrackedLookup for IndexStatusLookup<'_> {
    fn tracked_kind(&self, git_path: &[u8]) -> Option<StatusTrackedKind> {
        self.stat_cache.entries.get(git_path).map(|entry| {
            StatusTrackedKind::from_mode_and_skip(entry.mode, entry.is_skip_worktree())
        })
    }

    fn tracked_oid(&self, git_path: &[u8]) -> Option<ObjectId> {
        self.stat_cache.entries.get(git_path).map(|entry| entry.oid)
    }

    fn tracked_directory_kind(&self, git_path: &[u8]) -> Option<StatusTrackedDirectoryKind> {
        self.tracked_dirs
            .contains(git_path)
            .then_some(StatusTrackedDirectoryKind::ContainsTracked)
    }
}

pub(crate) struct BorrowedIndexLookup<'a> {
    entries: &'a [IndexEntryRef<'a>],
    exact_cursor: Cell<usize>,
    directory_prefix: RefCell<Vec<u8>>,
}

impl<'a> BorrowedIndexLookup<'a> {
    pub(crate) fn new(entries: &'a [IndexEntryRef<'a>]) -> Self {
        Self {
            entries,
            exact_cursor: Cell::new(0),
            directory_prefix: RefCell::new(Vec::new()),
        }
    }
}

impl StatusTrackedLookup for BorrowedIndexLookup<'_> {
    fn tracked_kind(&self, git_path: &[u8]) -> Option<StatusTrackedKind> {
        let mut start = self.exact_cursor.get().min(self.entries.len());
        if start == self.entries.len() || self.entries[start].path > git_path {
            start = self.entries.partition_point(|entry| entry.path < git_path);
        } else {
            while start < self.entries.len() && self.entries[start].path < git_path {
                start += 1;
            }
        }
        self.exact_cursor.set(start);
        self.entries[start..]
            .iter()
            .take_while(|entry| entry.path == git_path)
            .find(|entry| entry.stage() == Stage::Normal)
            .map(|entry| {
                StatusTrackedKind::from_mode_and_skip(entry.mode, entry.is_skip_worktree())
            })
    }

    fn tracked_oid(&self, git_path: &[u8]) -> Option<ObjectId> {
        let start = self.entries.partition_point(|entry| entry.path < git_path);
        self.entries[start..]
            .iter()
            .take_while(|entry| entry.path == git_path)
            .find(|entry| entry.stage() == Stage::Normal)
            .map(|entry| entry.oid)
    }

    fn tracked_directory_kind(&self, git_path: &[u8]) -> Option<StatusTrackedDirectoryKind> {
        let mut prefix_buf = self.directory_prefix.borrow_mut();
        prefix_buf.clear();
        prefix_buf.extend_from_slice(git_path);
        prefix_buf.push(b'/');
        let prefix = prefix_buf.as_slice();
        let start = self.entries.partition_point(|entry| entry.path < prefix);
        let mut saw_normal = false;
        for entry in self.entries[start..]
            .iter()
            .take_while(|entry| entry.path.starts_with(prefix))
        {
            if entry.stage() != Stage::Normal {
                continue;
            }
            saw_normal = true;
            if !entry.is_skip_worktree() {
                return Some(StatusTrackedDirectoryKind::ContainsTracked);
            }
        }
        saw_normal.then_some(StatusTrackedDirectoryKind::TrackedExcluded)
    }
}

pub(crate) struct StatusUntrackedWalk<'a, T: StatusTrackedLookup + ?Sized> {
    git_dir: &'a Path,
    tracked: &'a T,
    ignores: &'a mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&'a mut StatusProfileCounters>,
}

#[derive(Clone)]
pub(crate) struct StatusUntrackedFrontierTask {
    dir: PathBuf,
    git_path: Vec<u8>,
    ignores: IgnoreMatcher,
}

#[derive(Default)]
pub(crate) struct StatusUntrackedFrontierOutput {
    paths: Vec<Vec<u8>>,
    next: Vec<StatusUntrackedFrontierTask>,
    profile: StatusProfileCounters,
}

pub(crate) struct StatusDirEntry {
    name: std::ffi::OsString,
}

impl StatusDirEntry {
    fn file_name(&self) -> std::ffi::OsString {
        self.name.clone()
    }

    fn file_type_in(&self, dir: &Path) -> Result<fs::FileType> {
        Ok(fs::symlink_metadata(self.path_in(dir))?.file_type())
    }

    fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(&self.name)
    }
}

fn sort_status_dir_entries(entries: &mut [StatusDirEntry]) {
    entries.sort_by(|left, right| left.name.cmp(&right.name));
}

pub(crate) fn collect_status_untracked_paths<T: StatusTrackedLookup + ?Sized>(
    context: &mut StatusUntrackedWalk<'_, T>,
    dir: &Path,
    dir_git_path: &[u8],
    paths: &mut Vec<Vec<u8>>,
) -> Result<()> {
    if is_same_path(dir, context.git_dir) {
        return Ok(());
    }
    let ignore_len = context.ignores.patterns.len();
    let mut entries = read_status_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        context.git_dir,
        context.tracked,
        context.ignores,
        context.profile.as_deref_mut(),
    )?;
    sort_status_dir_entries(&mut entries);
    let result = (|| -> Result<()> {
        let mut git_path = dir_git_path.to_vec();
        for entry in entries {
            let file_name = entry.file_name();
            if file_name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let path_len = git_path_push_component(&mut git_path, &file_name);
            let entry_result = (|| -> Result<()> {
                if let Some(tracked_kind) = context.tracked.tracked_kind(&git_path) {
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.tracked_exact_hits += 1;
                    }
                    if !matches!(context.untracked_mode, StatusUntrackedMode::All)
                        || tracked_kind == StatusTrackedKind::Gitlink
                    {
                        return Ok(());
                    }
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.file_type_calls += 1;
                    }
                    let file_type = entry.file_type_in(dir)?;
                    if file_type.is_dir() {
                        let path = entry.path_in(dir);
                        if !is_same_path(&path, context.git_dir) {
                            collect_status_untracked_paths(context, &path, &git_path, paths)?;
                        }
                    }
                    return Ok(());
                }
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.file_type_calls += 1;
                }
                let file_type = entry.file_type_in(dir)?;
                let is_dir = file_type.is_dir();
                if file_type.is_file() || file_type.is_symlink() {
                    if !context.ignores.is_ignored_profiled(
                        &git_path,
                        false,
                        context.profile.as_deref_mut(),
                    ) {
                        paths.push(git_path.clone());
                    }
                    return Ok(());
                } else if is_dir {
                    let path = entry.path_in(dir);
                    if context.ignores.is_ignored_profiled(
                        &git_path,
                        true,
                        context.profile.as_deref_mut(),
                    ) {
                        return Ok(());
                    }
                    if is_same_path(&path, context.git_dir) {
                        return Ok(());
                    }
                    let tracked_directory = context.tracked.tracked_directory_kind(&git_path);
                    if let Some(directory_kind) = tracked_directory {
                        if let Some(profile) = context.profile.as_deref_mut() {
                            profile.tracked_dir_prefix_hits += 1;
                            if directory_kind == StatusTrackedDirectoryKind::TrackedExcluded {
                                profile.tracked_skip_worktree_prefix_hits += 1;
                            }
                        }
                    }
                    match context.untracked_mode {
                        StatusUntrackedMode::All => {
                            if tracked_directory.is_none()
                                && is_nested_repository_boundary(&path, context.git_dir)
                            {
                                push_untracked_directory(paths, &git_path);
                            } else {
                                collect_status_untracked_paths(context, &path, &git_path, paths)?;
                            }
                        }
                        StatusUntrackedMode::Normal => {
                            if tracked_directory.is_some() {
                                collect_status_untracked_paths(context, &path, &git_path, paths)?;
                            } else if is_nested_repository_boundary(&path, context.git_dir) {
                                push_untracked_directory(paths, &git_path);
                            } else if status_untracked_directory_has_file(
                                context, &path, &git_path,
                            )? {
                                push_untracked_directory(paths, &git_path);
                            }
                        }
                        StatusUntrackedMode::None => {}
                    }
                }
                Ok(())
            })();
            git_path.truncate(path_len);
            entry_result?;
        }
        Ok(())
    })();
    context.ignores.truncate(ignore_len);
    result
}

pub(crate) fn collect_status_untracked_frontier_dir<T: StatusTrackedLookup + ?Sized>(
    context: &mut StatusUntrackedWalk<'_, T>,
    dir: &Path,
    dir_git_path: &[u8],
    paths: &mut Vec<Vec<u8>>,
    next: &mut Vec<StatusUntrackedFrontierTask>,
) -> Result<()> {
    if is_same_path(dir, context.git_dir) {
        return Ok(());
    }
    let mut entries = read_status_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        context.git_dir,
        context.tracked,
        context.ignores,
        context.profile.as_deref_mut(),
    )?;
    sort_status_dir_entries(&mut entries);
    let mut git_path = dir_git_path.to_vec();
    for entry in entries {
        let file_name = entry.file_name();
        if file_name == std::ffi::OsStr::new(".git") {
            continue;
        }
        let path_len = git_path_push_component(&mut git_path, &file_name);
        let entry_result = (|| -> Result<()> {
            if let Some(tracked_kind) = context.tracked.tracked_kind(&git_path) {
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.tracked_exact_hits += 1;
                }
                if !matches!(context.untracked_mode, StatusUntrackedMode::All)
                    || tracked_kind == StatusTrackedKind::Gitlink
                {
                    return Ok(());
                }
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.file_type_calls += 1;
                }
                let file_type = entry.file_type_in(dir)?;
                if file_type.is_dir() {
                    let path = entry.path_in(dir);
                    if !is_same_path(&path, context.git_dir) {
                        next.push(StatusUntrackedFrontierTask {
                            dir: path,
                            git_path: git_path.clone(),
                            ignores: context.ignores.clone(),
                        });
                    }
                }
                return Ok(());
            }
            if let Some(profile) = context.profile.as_deref_mut() {
                profile.file_type_calls += 1;
            }
            let file_type = entry.file_type_in(dir)?;
            let is_dir = file_type.is_dir();
            if file_type.is_file() || file_type.is_symlink() {
                if !context.ignores.is_ignored_profiled(
                    &git_path,
                    false,
                    context.profile.as_deref_mut(),
                ) {
                    paths.push(git_path.clone());
                }
                return Ok(());
            } else if is_dir {
                let path = entry.path_in(dir);
                if context.ignores.is_ignored_profiled(
                    &git_path,
                    true,
                    context.profile.as_deref_mut(),
                ) {
                    return Ok(());
                }
                if is_same_path(&path, context.git_dir) {
                    return Ok(());
                }
                let tracked_directory = context.tracked.tracked_directory_kind(&git_path);
                if let Some(directory_kind) = tracked_directory {
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.tracked_dir_prefix_hits += 1;
                        if directory_kind == StatusTrackedDirectoryKind::TrackedExcluded {
                            profile.tracked_skip_worktree_prefix_hits += 1;
                        }
                    }
                }
                match context.untracked_mode {
                    StatusUntrackedMode::All => {
                        if tracked_directory.is_none()
                            && is_nested_repository_boundary(&path, context.git_dir)
                        {
                            push_untracked_directory(paths, &git_path);
                        } else {
                            next.push(StatusUntrackedFrontierTask {
                                dir: path,
                                git_path: git_path.clone(),
                                ignores: context.ignores.clone(),
                            });
                        }
                    }
                    StatusUntrackedMode::Normal => {
                        if tracked_directory.is_some() {
                            next.push(StatusUntrackedFrontierTask {
                                dir: path,
                                git_path: git_path.clone(),
                                ignores: context.ignores.clone(),
                            });
                        } else if is_nested_repository_boundary(&path, context.git_dir)
                            || status_untracked_directory_has_file(context, &path, &git_path)?
                        {
                            push_untracked_directory(paths, &git_path);
                        }
                    }
                    StatusUntrackedMode::None => {}
                }
            }
            Ok(())
        })();
        git_path.truncate(path_len);
        entry_result?;
    }
    Ok(())
}

pub(crate) fn stream_status_untracked_paths<T, F>(
    context: &mut StatusUntrackedWalk<'_, T>,
    dir: &Path,
    dir_git_path: &[u8],
    emit: &mut F,
) -> Result<StreamControl>
where
    T: StatusTrackedLookup + ?Sized,
    F: for<'a> FnMut(&'a [u8]) -> Result<StreamControl>,
{
    if is_same_path(dir, context.git_dir) {
        return Ok(StreamControl::Continue);
    }
    let ignore_len = context.ignores.patterns.len();
    let mut entries = read_status_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        context.git_dir,
        context.tracked,
        context.ignores,
        context.profile.as_deref_mut(),
    )?;
    sort_status_dir_entries(&mut entries);
    let result = (|| -> Result<StreamControl> {
        let mut git_path = dir_git_path.to_vec();
        for entry in entries {
            let file_name = entry.file_name();
            if file_name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let path_len = git_path_push_component(&mut git_path, &file_name);
            let entry_result = (|| -> Result<StreamControl> {
                if let Some(tracked_kind) = context.tracked.tracked_kind(&git_path) {
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.tracked_exact_hits += 1;
                    }
                    if !matches!(context.untracked_mode, StatusUntrackedMode::All)
                        || tracked_kind == StatusTrackedKind::Gitlink
                    {
                        return Ok(StreamControl::Continue);
                    }
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.file_type_calls += 1;
                    }
                    let file_type = entry.file_type_in(dir)?;
                    if file_type.is_dir() {
                        let path = entry.path_in(dir);
                        if !is_same_path(&path, context.git_dir) {
                            if stream_status_untracked_paths(context, &path, &git_path, emit)?
                                .is_stop()
                            {
                                return Ok(StreamControl::Stop);
                            }
                        }
                    }
                    return Ok(StreamControl::Continue);
                }
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.file_type_calls += 1;
                }
                let file_type = entry.file_type_in(dir)?;
                let is_dir = file_type.is_dir();
                if file_type.is_file() || file_type.is_symlink() {
                    if !context.ignores.is_ignored_profiled(
                        &git_path,
                        false,
                        context.profile.as_deref_mut(),
                    ) {
                        if emit_status_untracked_path(context, &git_path, emit)?.is_stop() {
                            return Ok(StreamControl::Stop);
                        }
                    }
                    return Ok(StreamControl::Continue);
                } else if is_dir {
                    if context.ignores.is_ignored_profiled(
                        &git_path,
                        true,
                        context.profile.as_deref_mut(),
                    ) {
                        return Ok(StreamControl::Continue);
                    }
                    let path = entry.path_in(dir);
                    if is_same_path(&path, context.git_dir) {
                        return Ok(StreamControl::Continue);
                    }
                    let tracked_directory = context.tracked.tracked_directory_kind(&git_path);
                    if let Some(directory_kind) = tracked_directory {
                        if let Some(profile) = context.profile.as_deref_mut() {
                            profile.tracked_dir_prefix_hits += 1;
                            if directory_kind == StatusTrackedDirectoryKind::TrackedExcluded {
                                profile.tracked_skip_worktree_prefix_hits += 1;
                            }
                        }
                    }
                    match context.untracked_mode {
                        StatusUntrackedMode::All => {
                            if tracked_directory.is_none()
                                && is_nested_repository_boundary(&path, context.git_dir)
                            {
                                let directory_len = git_path.len();
                                if git_path.last() != Some(&b'/') {
                                    git_path.push(b'/');
                                }
                                let control = emit_status_untracked_path(context, &git_path, emit)?;
                                git_path.truncate(directory_len);
                                if control.is_stop() {
                                    return Ok(StreamControl::Stop);
                                }
                            } else {
                                if stream_status_untracked_paths(context, &path, &git_path, emit)?
                                    .is_stop()
                                {
                                    return Ok(StreamControl::Stop);
                                }
                            }
                        }
                        StatusUntrackedMode::Normal => {
                            if tracked_directory.is_some() {
                                if stream_status_untracked_paths(context, &path, &git_path, emit)?
                                    .is_stop()
                                {
                                    return Ok(StreamControl::Stop);
                                }
                            } else if is_nested_repository_boundary(&path, context.git_dir)
                                || status_untracked_directory_has_file(context, &path, &git_path)?
                            {
                                let directory_len = git_path.len();
                                if git_path.last() != Some(&b'/') {
                                    git_path.push(b'/');
                                }
                                let control = emit_status_untracked_path(context, &git_path, emit)?;
                                git_path.truncate(directory_len);
                                if control.is_stop() {
                                    return Ok(StreamControl::Stop);
                                }
                            }
                        }
                        StatusUntrackedMode::None => {}
                    }
                }
                Ok(StreamControl::Continue)
            })();
            git_path.truncate(path_len);
            if entry_result?.is_stop() {
                return Ok(StreamControl::Stop);
            }
        }
        Ok(StreamControl::Continue)
    })();
    context.ignores.truncate(ignore_len);
    result
}

pub(crate) fn emit_status_untracked_path<T, F>(
    context: &mut StatusUntrackedWalk<'_, T>,
    path: &[u8],
    emit: &mut F,
) -> Result<StreamControl>
where
    T: StatusTrackedLookup + ?Sized,
    F: for<'a> FnMut(&'a [u8]) -> Result<StreamControl>,
{
    if let Some(profile) = context.profile.as_deref_mut() {
        profile.untracked_rows += 1;
    }
    emit(path)
}

pub(crate) fn stage0_tracked_directories(index: &Index) -> HashSet<&[u8]> {
    let mut directories = HashSet::new();
    for entry in index
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
    {
        let path = entry.path.as_bytes();
        for (idx, byte) in path.iter().enumerate() {
            if *byte == b'/' && idx > 0 {
                directories.insert(&path[..idx]);
            }
        }
    }
    directories
}

pub(crate) fn status_untracked_directory_has_file<T: StatusTrackedLookup + ?Sized>(
    context: &mut StatusUntrackedWalk<'_, T>,
    dir: &Path,
    dir_git_path: &[u8],
) -> Result<bool> {
    if is_same_path(dir, context.git_dir) {
        return Ok(false);
    }
    let ignore_len = context.ignores.patterns.len();
    let mut entries = read_status_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        context.git_dir,
        context.tracked,
        context.ignores,
        context.profile.as_deref_mut(),
    )?;
    sort_status_dir_entries(&mut entries);
    let result = (|| -> Result<bool> {
        let mut git_path = dir_git_path.to_vec();
        for entry in entries {
            let file_name = entry.file_name();
            if file_name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let path_len = git_path_push_component(&mut git_path, &file_name);
            let entry_result = (|| -> Result<Option<bool>> {
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.file_type_calls += 1;
                }
                let file_type = entry.file_type_in(dir)?;
                let is_dir = file_type.is_dir();
                if context.ignores.is_ignored_profiled(
                    &git_path,
                    is_dir,
                    context.profile.as_deref_mut(),
                ) {
                    return Ok(None);
                }
                if file_type.is_file() || file_type.is_symlink() {
                    return Ok(Some(true));
                }
                if is_dir {
                    let path = entry.path_in(dir);
                    if is_same_path(&path, context.git_dir) {
                        return Ok(None);
                    }
                    if is_nested_repository_boundary(&path, context.git_dir) {
                        return Ok(Some(true));
                    }
                    if status_untracked_directory_has_file(context, &path, &git_path)? {
                        return Ok(Some(true));
                    }
                }
                Ok(None)
            })();
            git_path.truncate(path_len);
            if let Some(has_file) = entry_result? {
                return Ok(has_file);
            }
        }
        Ok(false)
    })();
    context.ignores.truncate(ignore_len);
    result
}

pub(crate) fn read_dir_entries_with_ignore_patterns(
    dir: &Path,
    base: &[u8],
    matcher: &mut IgnoreMatcher,
    mut profile: Option<&mut StatusProfileCounters>,
) -> Result<Vec<StatusDirEntry>> {
    let mut entries = Vec::new();
    let mut ignore_path = None;
    if let Some(profile) = profile.as_deref_mut() {
        profile.read_dir_calls += 1;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(profile) = profile.as_deref_mut() {
            profile.dir_entries_seen += 1;
        }
        if name == std::ffi::OsStr::new(".gitignore") {
            ignore_path = Some(dir.join(&name));
        }
        entries.push(StatusDirEntry { name });
    }
    if let Some(profile) = profile {
        profile.read_dir_entry_vec_cap_bytes +=
            (entries.capacity() * std::mem::size_of::<StatusDirEntry>()) as u64;
        profile.read_dir_entry_vec_max_len =
            profile.read_dir_entry_vec_max_len.max(entries.len() as u64);
        profile.read_dir_entry_vec_max_cap = profile
            .read_dir_entry_vec_max_cap
            .max(entries.capacity() as u64);
        profile.read_dir_name_vec_cap_bytes += entries
            .iter()
            .map(|entry| entry.name.capacity() as u64)
            .sum::<u64>();
        profile.read_dir_name_vec_max_len = profile.read_dir_name_vec_max_len.max(
            entries
                .iter()
                .map(|entry| entry.name.len() as u64)
                .max()
                .unwrap_or(0),
        );
        profile.read_dir_name_vec_max_cap = profile.read_dir_name_vec_max_cap.max(
            entries
                .iter()
                .map(|entry| entry.name.capacity() as u64)
                .max()
                .unwrap_or(0),
        );
    }
    if let Some(path) = ignore_path {
        let mut source = base.to_vec();
        if !source.is_empty() {
            source.push(b'/');
        }
        source.extend_from_slice(b".gitignore");
        read_per_directory_ignore_patterns_into_matcher(path, matcher, base, &source)?;
    }
    Ok(entries)
}

pub(crate) fn read_status_dir_entries_with_ignore_patterns<T: StatusTrackedLookup + ?Sized>(
    dir: &Path,
    base: &[u8],
    git_dir: &Path,
    tracked: &T,
    matcher: &mut IgnoreMatcher,
    profile: Option<&mut StatusProfileCounters>,
) -> Result<Vec<StatusDirEntry>> {
    read_skip_worktree_ignore_patterns(git_dir, base, tracked, matcher)?;
    read_dir_entries_with_ignore_patterns(dir, base, matcher, profile)
}

fn read_skip_worktree_ignore_patterns<T: StatusTrackedLookup + ?Sized>(
    git_dir: &Path,
    base: &[u8],
    tracked: &T,
    matcher: &mut IgnoreMatcher,
) -> Result<()> {
    let mut source = base.to_vec();
    if !source.is_empty() {
        source.push(b'/');
    }
    source.extend_from_slice(b".gitignore");
    if tracked.tracked_kind(&source) != Some(StatusTrackedKind::SkipWorktree) {
        return Ok(());
    }
    let Some(oid) = tracked.tracked_oid(&source) else {
        return Ok(());
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, oid.format());
    let object = read_expected_object(&db, &oid, ObjectType::Blob)?;
    for (line, raw) in object.body.split(|byte| *byte == b'\n').enumerate() {
        matcher.push_raw_pattern(raw, base, &source, line + 1);
    }
    Ok(())
}

pub(crate) fn build_untracked_cache(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
    untracked_mode: StatusUntrackedMode,
) -> Result<UntrackedCache> {
    let stat_cache = IndexStatCache::from_index(index, &repository_index_path(git_dir));
    let tracked_dirs = stage0_tracked_directories(index);
    let tracked = IndexStatusLookup {
        stat_cache: &stat_cache,
        tracked_dirs: &tracked_dirs,
    };
    let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
    let mut cache = UntrackedCache::new(
        format,
        untracked_cache_ident(worktree_root),
        untracked_cache_dir_flags(untracked_mode),
    );
    cache.info_exclude = untracked_cache_oid_stat(&git_dir.join("info").join("exclude"), format)?;
    cache.excludes_file = UntrackedCacheOidStat::new(format);
    cache.root = Some(build_untracked_cache_dir(
        worktree_root,
        git_dir,
        worktree_root,
        &[],
        b"",
        &tracked,
        &mut ignores,
        untracked_mode,
        format,
        false,
    )?);
    Ok(cache)
}

pub(crate) fn emit_untracked_cache_trace(old: Option<&UntrackedCache>, new: &UntrackedCache) {
    sley_core::trace2::perf_read_directory_data("path", "");
    let dir_count = new
        .root
        .as_ref()
        .map(count_untracked_cache_dirs)
        .unwrap_or(0);
    let Some(old) = old else {
        sley_core::trace2::perf_read_directory_data("node-creation", dir_count.saturating_sub(1));
        sley_core::trace2::perf_read_directory_data("gitignore-invalidation", 1);
        sley_core::trace2::perf_read_directory_data("directory-invalidation", 0);
        sley_core::trace2::perf_read_directory_data("opendir", dir_count);
        return;
    };
    let Some(old_root) = old.root.as_ref() else {
        sley_core::trace2::perf_read_directory_data("node-creation", dir_count.saturating_sub(1));
        sley_core::trace2::perf_read_directory_data("gitignore-invalidation", 1);
        sley_core::trace2::perf_read_directory_data("directory-invalidation", 0);
        sley_core::trace2::perf_read_directory_data("opendir", dir_count);
        return;
    };
    let Some(new_root) = new.root.as_ref() else {
        return;
    };
    if old.ident != new.ident || old.dir_flags != new.dir_flags {
        sley_core::trace2::perf_read_directory_data("node-creation", dir_count.saturating_sub(1));
        sley_core::trace2::perf_read_directory_data("gitignore-invalidation", 1);
        sley_core::trace2::perf_read_directory_data("directory-invalidation", 0);
        sley_core::trace2::perf_read_directory_data("opendir", dir_count);
        return;
    }
    if old.info_exclude.oid != new.info_exclude.oid
        || old.excludes_file.oid != new.excludes_file.oid
    {
        sley_core::trace2::perf_read_directory_data("node-creation", 0);
        sley_core::trace2::perf_read_directory_data("gitignore-invalidation", 1);
        sley_core::trace2::perf_read_directory_data("directory-invalidation", 0);
        sley_core::trace2::perf_read_directory_data("opendir", dir_count);
        return;
    }
    let mut events = UntrackedCacheTraversalEvents::default();
    collect_untracked_cache_traversal_events(Some(old_root), new_root, false, true, &mut events);
    sley_core::trace2::perf_read_directory_data("node-creation", events.node_creation);
    sley_core::trace2::perf_read_directory_data(
        "gitignore-invalidation",
        events.gitignore_invalidation,
    );
    sley_core::trace2::perf_read_directory_data(
        "directory-invalidation",
        events.directory_invalidation,
    );
    sley_core::trace2::perf_read_directory_data("opendir", events.opendir);
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UntrackedCacheTraversalEvents {
    pub(crate) node_creation: usize,
    pub(crate) gitignore_invalidation: usize,
    pub(crate) directory_invalidation: usize,
    pub(crate) opendir: usize,
}

/// Derive trace2 read-directory counters from the old/new canonical UNTR
/// trees. New nodes are opened; a changed directory stat opens that directory;
/// an invalid node is reopened; and a changed per-directory ignore source
/// forces its whole cached subtree open.
pub(crate) fn collect_untracked_cache_traversal_events(
    old: Option<&UntrackedCacheDir>,
    new: &UntrackedCacheDir,
    ancestor_ignore_invalidated: bool,
    is_root: bool,
    events: &mut UntrackedCacheTraversalEvents,
) {
    let is_new = old.is_none();
    if is_new && !is_root {
        events.node_creation += 1;
    }
    let exclude_changed = old.is_some_and(|old| old.exclude_oid != new.exclude_oid);
    let was_invalid = old.is_some_and(|old| !old.valid);
    // Explicit index invalidation already requires reopening the node; Git does
    // not also classify its inevitably stale stat as a directory invalidation.
    let stat_changed = old.is_some_and(|old| old.valid && old.stat != new.stat);
    if exclude_changed {
        events.gitignore_invalidation += 1;
    }
    if stat_changed {
        events.directory_invalidation += 1;
    }
    if ancestor_ignore_invalidated || is_new || exclude_changed || stat_changed || was_invalid {
        events.opendir += 1;
    }

    let force_children = ancestor_ignore_invalidated || is_new || exclude_changed;
    for new_child in &new.dirs {
        let old_child = old.and_then(|old| {
            old.dirs
                .iter()
                .find(|old_child| old_child.name == new_child.name)
        });
        collect_untracked_cache_traversal_events(
            old_child,
            new_child,
            force_children,
            false,
            events,
        );
    }
}

pub(crate) fn count_untracked_cache_dirs(dir: &UntrackedCacheDir) -> usize {
    1 + dir
        .dirs
        .iter()
        .map(count_untracked_cache_dirs)
        .sum::<usize>()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_untracked_cache_dir<T: StatusTrackedLookup + ?Sized>(
    worktree_root: &Path,
    git_dir: &Path,
    dir: &Path,
    dir_git_path: &[u8],
    name: &[u8],
    tracked: &T,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    format: ObjectFormat,
    check_only: bool,
) -> Result<UntrackedCacheDir> {
    let ignore_len = ignores.patterns.len();
    let mut entries = read_status_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        git_dir,
        tracked,
        ignores,
        None,
    )?;
    sort_status_dir_entries(&mut entries);
    let exclude_path = if dir_git_path.is_empty() {
        b".gitignore".to_vec()
    } else {
        let mut path = dir_git_path.to_vec();
        path.push(b'/');
        path.extend_from_slice(b".gitignore");
        path
    };
    let exclude_kind = tracked.tracked_kind(&exclude_path);
    let exclude_oid = match exclude_kind {
        // A skip-worktree .gitignore is absent from disk by design. Git carries
        // its blob id into the UNTR node so sparse status still invalidates and
        // applies the correct per-directory rules without materializing it.
        Some(StatusTrackedKind::SkipWorktree) => tracked.tracked_oid(&exclude_path),
        Some(StatusTrackedKind::File | StatusTrackedKind::Gitlink) => None,
        None => per_directory_ignore_oid(dir, format)?,
    };
    let mut node = UntrackedCacheDir {
        name: name.to_vec(),
        stat: fs::symlink_metadata(dir)
            .map(|metadata| untracked_cache_stat_data(&metadata))
            .unwrap_or_default(),
        exclude_oid,
        valid: true,
        check_only,
        recurse: true,
        ..UntrackedCacheDir::default()
    };
    let result = (|| -> Result<()> {
        let mut git_path = dir_git_path.to_vec();
        for entry in entries {
            let file_name = entry.file_name();
            if file_name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let path_len = git_path_push_component(&mut git_path, &file_name);
            let entry_result = (|| -> Result<()> {
                if tracked.tracked_kind(&git_path).is_some() {
                    return Ok(());
                }
                let file_type = entry.file_type_in(dir)?;
                let is_dir = file_type.is_dir();
                if ignores.is_ignored(&git_path, is_dir) {
                    return Ok(());
                }
                if file_type.is_file() || file_type.is_symlink() {
                    node.untracked.push(component_name_bytes(&file_name));
                    return Ok(());
                }
                if !is_dir {
                    return Ok(());
                }
                let path = entry.path_in(dir);
                if is_same_path(&path, git_dir) {
                    return Ok(());
                }
                let component = component_name_bytes(&file_name);
                let tracked_directory = tracked.tracked_directory_kind(&git_path);
                let child_check_only = matches!(untracked_mode, StatusUntrackedMode::Normal)
                    && tracked_directory.is_none();
                let child = build_untracked_cache_dir(
                    worktree_root,
                    git_dir,
                    &path,
                    &git_path,
                    &component,
                    tracked,
                    ignores,
                    untracked_mode,
                    format,
                    child_check_only,
                )?;
                let child_has_untracked = !child.untracked.is_empty()
                    || child
                        .dirs
                        .iter()
                        .any(|dir| !dir.untracked.is_empty() || !dir.dirs.is_empty());
                match untracked_mode {
                    StatusUntrackedMode::All => {
                        node.dirs.push(child);
                    }
                    StatusUntrackedMode::Normal => {
                        if tracked_directory.is_some() {
                            node.dirs.push(child);
                        } else {
                            if child_has_untracked {
                                let mut directory = component.clone();
                                directory.push(b'/');
                                node.untracked.push(directory);
                            }
                            node.dirs.push(child);
                        }
                    }
                    StatusUntrackedMode::None => {}
                }
                Ok(())
            })();
            git_path.truncate(path_len);
            entry_result?;
        }
        Ok(())
    })();
    ignores.truncate(ignore_len);
    result?;
    if exclude_kind == Some(StatusTrackedKind::SkipWorktree)
        && !untracked_cache_dir_has_observed_content(&node)
    {
        // Git loads a sparse/skip-worktree ignore blob to classify the walk,
        // but leaves the UNTR node's exclude OID null until that directory has
        // relevant untracked content. The first later population then records
        // the blob and is a real gitignore-invalidation traversal event.
        node.exclude_oid = None;
    }
    if worktree_root == dir {
        node.name.clear();
    }
    Ok(node)
}

fn untracked_cache_dir_has_observed_content(dir: &UntrackedCacheDir) -> bool {
    !dir.untracked.is_empty()
        || dir
            .dirs
            .iter()
            .any(untracked_cache_dir_has_observed_content)
}

pub(crate) fn component_name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().as_bytes().to_vec()
    }
}

pub(crate) fn per_directory_ignore_oid(
    dir: &Path,
    format: ObjectFormat,
) -> Result<Option<ObjectId>> {
    let path = dir.join(".gitignore");
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(untracked_cache_exclude_oid(bytes, format)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn untracked_cache_oid_stat(
    path: &Path,
    format: ObjectFormat,
) -> Result<UntrackedCacheOidStat> {
    let stat = fs::symlink_metadata(path)
        .map(|metadata| untracked_cache_stat_data(&metadata))
        .unwrap_or_default();
    let oid = match fs::read(path) {
        Ok(bytes) => untracked_cache_exclude_oid(bytes, format)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => ObjectId::null(format),
        Err(err) => return Err(err.into()),
    };
    Ok(UntrackedCacheOidStat { stat, oid })
}

pub(crate) fn untracked_cache_exclude_oid(
    mut bytes: Vec<u8>,
    format: ObjectFormat,
) -> Result<ObjectId> {
    if !bytes.is_empty() {
        bytes.push(b'\n');
    }
    EncodedObject::new(ObjectType::Blob, bytes).object_id(format)
}

#[cfg(unix)]
pub(crate) fn untracked_cache_stat_data(metadata: &fs::Metadata) -> UntrackedCacheStatData {
    use std::os::unix::fs::MetadataExt;
    UntrackedCacheStatData {
        ctime_seconds: metadata.ctime().min(u32::MAX as i64).max(0) as u32,
        ctime_nanoseconds: metadata.ctime_nsec().min(u32::MAX as i64).max(0) as u32,
        mtime_seconds: metadata.mtime().min(u32::MAX as i64).max(0) as u32,
        mtime_nanoseconds: metadata.mtime_nsec().min(u32::MAX as i64).max(0) as u32,
        dev: metadata.dev() as u32,
        ino: metadata.ino() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        size: metadata.size().min(u32::MAX as u64) as u32,
    }
}

#[cfg(not(unix))]
pub(crate) fn untracked_cache_stat_data(metadata: &fs::Metadata) -> UntrackedCacheStatData {
    let (mtime_seconds, mtime_nanoseconds) = file_mtime_parts(metadata).unwrap_or((0, 0));
    UntrackedCacheStatData {
        mtime_seconds: mtime_seconds.min(u64::from(u32::MAX)) as u32,
        mtime_nanoseconds: mtime_nanoseconds.min(u64::from(u32::MAX)) as u32,
        size: metadata.len().min(u64::from(u32::MAX)) as u32,
        ..UntrackedCacheStatData::default()
    }
}

pub(crate) fn untracked_cache_dir_flags(untracked_mode: StatusUntrackedMode) -> u32 {
    match untracked_mode {
        StatusUntrackedMode::All => 0,
        StatusUntrackedMode::Normal | StatusUntrackedMode::None => {
            sley_index::untracked_cache_normal_flags()
        }
    }
}

pub(crate) fn untracked_cache_ident(worktree_root: &Path) -> Vec<u8> {
    let mut ident = format!(
        "Location {}, system {}",
        worktree_root.display(),
        untracked_cache_system_name()
    )
    .into_bytes();
    ident.push(0);
    ident
}

pub(crate) fn untracked_cache_system_name() -> String {
    fs::read_to_string("/proc/sys/kernel/ostype")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            let os = std::env::consts::OS;
            let mut chars = os.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => "Unknown".to_string(),
            }
        })
}

pub(crate) fn push_untracked_directory(paths: &mut Vec<Vec<u8>>, git_path: &[u8]) {
    paths.push(untracked_directory_path(git_path));
}

pub(crate) fn untracked_directory_path(git_path: &[u8]) -> Vec<u8> {
    let mut directory = git_path.to_vec();
    if directory.last() != Some(&b'/') {
        directory.push(b'/');
    }
    directory
}

pub(crate) fn untracked_normal_rollup_path(
    file_path: &[u8],
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Vec<u8> {
    let segments = file_path
        .split(|byte| *byte == b'/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() <= 1 {
        return file_path.to_vec();
    }
    let mut prefix = Vec::new();
    for segment in &segments[..segments.len() - 1] {
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(segment);
        if index_has_path_under(index, &prefix) {
            // This directory itself cannot be rolled up because it contains
            // tracked entries, but a wholly-untracked child still can. Keep
            // walking until the first untracked directory below the tracked
            // prefix instead of falling back to the leaf path.
            continue;
        }
        if !ignores.is_ignored(&prefix, true) {
            let mut directory = prefix;
            directory.push(b'/');
            return directory;
        }
    }
    file_path.to_vec()
}

pub(crate) fn ignored_traditional_rollup_path(
    root: &Path,
    git_dir: &Path,
    path: &[u8],
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Result<Vec<u8>> {
    let rolled = untracked_normal_rollup_path(path, index, ignores);
    if rolled == path {
        return Ok(rolled);
    }
    let Some(directory_path) = rolled.strip_suffix(b"/") else {
        return Ok(rolled);
    };
    if ignores.is_ignored(directory_path, true) {
        return Ok(rolled);
    }
    let mut absolute = PathBuf::new();
    set_worktree_path_from_repo_path(root, directory_path, &mut absolute)?;
    if directory_has_file(&absolute, root, git_dir, ignores)? {
        return Ok(path.to_vec());
    }
    Ok(rolled)
}

pub(crate) fn directory_has_file(
    dir: &Path,
    root: &Path,
    git_dir: &Path,
    ignores: &IgnoreMatcher,
) -> Result<bool> {
    if is_same_path(dir, git_dir) {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_embedded_git_internals(root, &path) {
            continue;
        }
        if is_same_path(&path, git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let relative = path.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if ignores.is_ignored(&git_path, metadata.is_dir()) {
            continue;
        }
        if metadata.is_file() || metadata.file_type().is_symlink() {
            return Ok(true);
        }
        if metadata.is_dir() {
            if is_nested_repository_boundary(&path, git_dir) {
                continue;
            }
            if directory_has_file(&path, root, git_dir, ignores)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn directory_has_ignored(
    dir: &Path,
    root: &Path,
    git_dir: &Path,
    ignores: &IgnoreMatcher,
) -> Result<bool> {
    if is_same_path(dir, git_dir) {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_same_path(&path, git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let relative = path.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if ignores.is_ignored(&git_path, metadata.is_dir()) {
            return Ok(true);
        }
        if metadata.is_dir() && directory_has_ignored(&path, root, git_dir, ignores)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn ignored_untracked_paths(
    root: &Path,
    git_dir: &Path,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
    directory: bool,
) -> Result<Vec<Vec<u8>>> {
    let mut paths = BTreeSet::new();
    let context = IgnoredUntrackedContext {
        root,
        git_dir,
        index,
        ignores,
        directory,
    };
    collect_ignored_untracked_paths(&context, root, false, &mut paths)?;
    Ok(paths.into_iter().collect())
}

pub(crate) fn ignored_traditional_path_is_empty_directory(
    root: &Path,
    path: &[u8],
) -> Result<bool> {
    let Some(path) = path.strip_suffix(b"/") else {
        return Ok(false);
    };
    let mut absolute = PathBuf::new();
    set_worktree_path_from_repo_path(root, path, &mut absolute)?;
    match fs::read_dir(&absolute) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub(crate) struct IgnoredUntrackedContext<'a> {
    root: &'a Path,
    git_dir: &'a Path,
    index: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &'a IgnoreMatcher,
    directory: bool,
}

pub(crate) fn collect_ignored_untracked_paths(
    context: &IgnoredUntrackedContext<'_>,
    dir: &Path,
    parent_ignored: bool,
    paths: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    if is_same_path(dir, context.git_dir) {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_same_path(&path, context.git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let relative = path.strip_prefix(context.root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if metadata.is_dir() {
            let ignored = parent_ignored || context.ignores.is_ignored(&git_path, true);
            if ignored && !index_has_path_under(context.index, &git_path) {
                if context.directory || is_nested_repository_boundary(&path, context.git_dir) {
                    let mut directory_path = git_path;
                    directory_path.push(b'/');
                    paths.insert(directory_path);
                } else {
                    collect_ignored_untracked_paths(context, &path, true, paths)?;
                }
            } else {
                if is_nested_repository_boundary(&path, context.git_dir) {
                    continue;
                }
                collect_ignored_untracked_paths(context, &path, ignored, paths)?;
            }
        } else if !context.index.contains_key(&git_path)
            && (metadata.is_file() || metadata.file_type().is_symlink())
            && (parent_ignored || context.ignores.is_ignored(&git_path, false))
        {
            paths.insert(git_path);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct IgnoreMatcher {
    pub(crate) patterns: Vec<IgnorePattern>,
    pub(crate) buckets: IgnorePatternBuckets,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct IgnorePatternBuckets {
    pub(crate) literal_basename: HashMap<Vec<u8>, Vec<usize>>,
    pub(crate) directory_literal_basename: HashMap<Vec<u8>, Vec<usize>>,
    pub(crate) literal_path_basename: HashMap<Vec<u8>, Vec<usize>>,
    pub(crate) directory_literal_path_basename: HashMap<Vec<u8>, Vec<usize>>,
    pub(crate) path_suffix_basename: HashMap<Vec<u8>, Vec<usize>>,
    pub(crate) directory_path_suffix_basename: HashMap<Vec<u8>, Vec<usize>>,
    pub(crate) glob_path_literal_basename: HashMap<Vec<u8>, Vec<usize>>,
    pub(crate) glob_directory_literal_basename: HashMap<Vec<u8>, Vec<usize>>,
    pub(crate) glob_path_suffix_basename: Vec<usize>,
    pub(crate) glob_path_prefix_basename: Vec<usize>,
    pub(crate) glob_directory_suffix_basename: Vec<usize>,
    pub(crate) glob_directory_prefix_basename: Vec<usize>,
    pub(crate) suffix_basename: HashMap<u8, Vec<usize>>,
    pub(crate) prefix_basename: HashMap<u8, Vec<usize>>,
    pub(crate) other: Vec<usize>,
}

impl IgnorePatternBuckets {
    fn push(&mut self, index: usize, pattern: &IgnorePattern) {
        match pattern.bucket_kind() {
            IgnoreBucketKind::LiteralBasename => self
                .literal_basename
                .entry(pattern.pattern.clone())
                .or_default()
                .push(index),
            IgnoreBucketKind::DirectoryLiteralBasename => self
                .directory_literal_basename
                .entry(pattern.pattern.clone())
                .or_default()
                .push(index),
            IgnoreBucketKind::LiteralPathBasename => self
                .literal_path_basename
                .entry(path_basename(&pattern.pattern).to_vec())
                .or_default()
                .push(index),
            IgnoreBucketKind::DirectoryLiteralPathBasename => self
                .directory_literal_path_basename
                .entry(path_basename(&pattern.pattern).to_vec())
                .or_default()
                .push(index),
            IgnoreBucketKind::PathSuffixBasename => {
                let suffix = pattern
                    .pattern
                    .strip_prefix(b"**/")
                    .unwrap_or(&pattern.pattern);
                self.path_suffix_basename
                    .entry(path_basename(suffix).to_vec())
                    .or_default()
                    .push(index);
            }
            IgnoreBucketKind::DirectoryPathSuffixBasename => {
                let suffix = pattern
                    .pattern
                    .strip_prefix(b"**/")
                    .unwrap_or(&pattern.pattern);
                self.directory_path_suffix_basename
                    .entry(path_basename(suffix).to_vec())
                    .or_default()
                    .push(index);
            }
            IgnoreBucketKind::GlobPathLiteralBasename => self
                .glob_path_literal_basename
                .entry(path_basename(&pattern.pattern).to_vec())
                .or_default()
                .push(index),
            IgnoreBucketKind::GlobDirectoryLiteralBasename => self
                .glob_directory_literal_basename
                .entry(path_basename(&pattern.pattern).to_vec())
                .or_default()
                .push(index),
            IgnoreBucketKind::GlobPathSuffixBasename => self.glob_path_suffix_basename.push(index),
            IgnoreBucketKind::GlobPathPrefixBasename => self.glob_path_prefix_basename.push(index),
            IgnoreBucketKind::GlobDirectorySuffixBasename => {
                self.glob_directory_suffix_basename.push(index)
            }
            IgnoreBucketKind::GlobDirectoryPrefixBasename => {
                self.glob_directory_prefix_basename.push(index)
            }
            IgnoreBucketKind::SuffixBasename => {
                if let Some(last) = pattern.pattern.last() {
                    self.suffix_basename.entry(*last).or_default().push(index);
                }
            }
            IgnoreBucketKind::PrefixBasename => self
                .prefix_basename
                .entry(pattern.pattern[0])
                .or_default()
                .push(index),
            IgnoreBucketKind::Other => self.other.push(index),
        }
    }

    fn truncate(&mut self, len: usize) {
        fn truncate_indices(indices: &mut Vec<usize>, len: usize) {
            let keep = indices.partition_point(|index| *index < len);
            indices.truncate(keep);
        }
        for indices in self.literal_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.directory_literal_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.literal_path_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.directory_literal_path_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.path_suffix_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.directory_path_suffix_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.glob_path_literal_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.glob_directory_literal_basename.values_mut() {
            truncate_indices(indices, len);
        }
        truncate_indices(&mut self.glob_path_suffix_basename, len);
        truncate_indices(&mut self.glob_path_prefix_basename, len);
        truncate_indices(&mut self.glob_directory_suffix_basename, len);
        truncate_indices(&mut self.glob_directory_prefix_basename, len);
        for indices in self.suffix_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.prefix_basename.values_mut() {
            truncate_indices(indices, len);
        }
        truncate_indices(&mut self.other, len);
    }

    fn profile_map_count(&self) -> usize {
        self.literal_basename.len()
            + self.directory_literal_basename.len()
            + self.literal_path_basename.len()
            + self.directory_literal_path_basename.len()
            + self.path_suffix_basename.len()
            + self.directory_path_suffix_basename.len()
            + self.glob_path_literal_basename.len()
            + self.glob_directory_literal_basename.len()
            + self.suffix_basename.len()
            + self.prefix_basename.len()
    }

    fn profile_index_count(&self) -> usize {
        fn map_indices<K>(map: &HashMap<K, Vec<usize>>) -> usize {
            map.values().map(Vec::len).sum()
        }
        map_indices(&self.literal_basename)
            + map_indices(&self.directory_literal_basename)
            + map_indices(&self.literal_path_basename)
            + map_indices(&self.directory_literal_path_basename)
            + map_indices(&self.path_suffix_basename)
            + map_indices(&self.directory_path_suffix_basename)
            + map_indices(&self.glob_path_literal_basename)
            + map_indices(&self.glob_directory_literal_basename)
            + self.glob_path_suffix_basename.len()
            + self.glob_path_prefix_basename.len()
            + self.glob_directory_suffix_basename.len()
            + self.glob_directory_prefix_basename.len()
            + map_indices(&self.suffix_basename)
            + map_indices(&self.prefix_basename)
            + self.other.len()
    }

    fn profile_index_vec_bytes(&self) -> usize {
        fn map_bytes<K>(map: &HashMap<K, Vec<usize>>) -> usize {
            map.values()
                .map(|indices| indices.capacity() * std::mem::size_of::<usize>())
                .sum()
        }
        map_bytes(&self.literal_basename)
            + map_bytes(&self.directory_literal_basename)
            + map_bytes(&self.literal_path_basename)
            + map_bytes(&self.directory_literal_path_basename)
            + map_bytes(&self.path_suffix_basename)
            + map_bytes(&self.directory_path_suffix_basename)
            + map_bytes(&self.glob_path_literal_basename)
            + map_bytes(&self.glob_directory_literal_basename)
            + self.glob_path_suffix_basename.capacity() * std::mem::size_of::<usize>()
            + self.glob_path_prefix_basename.capacity() * std::mem::size_of::<usize>()
            + self.glob_directory_suffix_basename.capacity() * std::mem::size_of::<usize>()
            + self.glob_directory_prefix_basename.capacity() * std::mem::size_of::<usize>()
            + map_bytes(&self.suffix_basename)
            + map_bytes(&self.prefix_basename)
            + self.other.capacity() * std::mem::size_of::<usize>()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct IgnorePattern {
    pub(crate) base: Vec<u8>,
    pub(crate) pattern: Vec<u8>,
    pub(crate) original: Vec<u8>,
    pub(crate) source: Vec<u8>,
    pub(crate) line_number: usize,
    pub(crate) negated: bool,
    pub(crate) directory_only: bool,
    pub(crate) anchored: bool,
    pub(crate) has_slash: bool,
    /// How `pattern` should be matched against a slash-free segment. Most
    /// `.gitignore` entries are literals or simple `*.ext` / `prefix*` globs, all
    /// of which match without the allocating wildcard DP engine; only genuinely
    /// complex globs fall through to [`wildcard_path_matches`].
    pub(crate) match_kind: MatchKind,
    pub(crate) glob_literal_prefix_len: usize,
}

/// Classification of an [`IgnorePattern`] that lets common shapes skip the
/// general wildcard matcher. Literal/prefix/suffix variants match a slash-free
/// segment; [`MatchKind::PathSuffix`] handles the common `**/literal/path`
/// shape, and the remaining complex patterns defer to the full engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchKind {
    /// No metacharacters: matches by byte equality.
    Literal,
    /// `*X` with `X` literal: matches a segment ending in `X`.
    Suffix,
    /// `X*` with `X` literal: matches a segment starting with `X`.
    Prefix,
    /// `**/X/Y` with a literal suffix: matches a path ending at `X/Y`.
    PathSuffix,
    /// Anything else: defer to [`wildcard_path_matches`].
    Glob,
}

pub(crate) fn path_basename(path: &[u8]) -> &[u8] {
    path.rsplit(|byte| *byte == b'/').next().unwrap_or(path)
}

pub(crate) fn path_component_has_glob_meta(component: &[u8]) -> bool {
    component
        .iter()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\'))
}

pub(crate) fn final_component_match_kind(pattern: &[u8]) -> MatchKind {
    classify_ignore_pattern(path_basename(pattern))
}

pub(crate) fn visit_directory_match_components(
    path: &[u8],
    is_dir: bool,
    mut visit: impl FnMut(&[u8]),
) {
    let mut start = 0usize;
    for (index, byte) in path.iter().enumerate() {
        if *byte == b'/' {
            if index > start {
                visit(&path[start..index]);
            }
            start = index + 1;
        }
    }
    if is_dir && start < path.len() {
        visit(&path[start..]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreBucketKind {
    LiteralBasename,
    DirectoryLiteralBasename,
    LiteralPathBasename,
    DirectoryLiteralPathBasename,
    PathSuffixBasename,
    DirectoryPathSuffixBasename,
    GlobPathLiteralBasename,
    GlobDirectoryLiteralBasename,
    GlobPathSuffixBasename,
    GlobPathPrefixBasename,
    GlobDirectorySuffixBasename,
    GlobDirectoryPrefixBasename,
    SuffixBasename,
    PrefixBasename,
    Other,
}

/// Classify `pattern` for [`MatchKind`]. `*X`/`X*` fast paths require the literal
/// part to be slash-free so that `ends_with`/`starts_with` on a single segment is
/// exactly equivalent to the glob (`*` never crosses `/`).
pub(crate) fn classify_ignore_pattern(pattern: &[u8]) -> MatchKind {
    if let Some(suffix) = pattern.strip_prefix(b"**/")
        && !suffix.is_empty()
        && !suffix
            .iter()
            .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\'))
    {
        return MatchKind::PathSuffix;
    }
    let stars = pattern.iter().filter(|byte| **byte == b'*').count();
    let other_meta = pattern
        .iter()
        .any(|byte| matches!(byte, b'?' | b'[' | b'\\'));
    if stars == 0 && !other_meta {
        return MatchKind::Literal;
    }
    if stars == 1 && !other_meta {
        let literal = if pattern.first() == Some(&b'*') {
            Some((&pattern[1..], MatchKind::Suffix))
        } else if pattern.last() == Some(&b'*') {
            Some((&pattern[..pattern.len() - 1], MatchKind::Prefix))
        } else {
            None
        };
        if let Some((literal, kind)) = literal
            && !literal.is_empty()
            && !literal.contains(&b'/')
        {
            return kind;
        }
    }
    MatchKind::Glob
}

impl IgnoreMatcher {
    pub(crate) fn emit_memory_profile(&self, label: &str) {
        let pattern_payload_bytes = self
            .patterns
            .iter()
            .map(|pattern| {
                pattern.base.capacity()
                    + pattern.pattern.capacity()
                    + pattern.original.capacity()
                    + pattern.source.capacity()
            })
            .sum();
        status_profile_mem(
            label,
            &[
                ("ignore_patterns_len", self.patterns.len()),
                ("ignore_patterns_cap", self.patterns.capacity()),
                (
                    "ignore_pattern_struct_bytes",
                    self.patterns.capacity() * std::mem::size_of::<IgnorePattern>(),
                ),
                ("ignore_pattern_payload_bytes", pattern_payload_bytes),
                ("ignore_bucket_map_count", self.buckets.profile_map_count()),
                (
                    "ignore_bucket_index_count",
                    self.buckets.profile_index_count(),
                ),
                (
                    "ignore_bucket_index_vec_bytes",
                    self.buckets.profile_index_vec_bytes(),
                ),
            ],
        );
    }

    fn from_sources(
        root: &Path,
        exclude_standard: bool,
        patterns: &[Vec<u8>],
        per_directory: &[String],
    ) -> Result<Self> {
        let mut matcher = if exclude_standard {
            Self::from_worktree_root(root)?
        } else {
            Self::default()
        };
        matcher.extend_patterns(patterns);
        matcher.extend_per_directory_patterns(root, per_directory)?;
        Ok(matcher)
    }

    /// Builds only the repository-wide ignore sources — `core.excludesFile` (or the
    /// default global) and `$GIT_DIR/info/exclude` — *without* walking the worktree
    /// for `.gitignore`. The caller folds each directory's `.gitignore` into the
    /// matcher as it descends (see [`read_dir_ignore_patterns`]), so status reads
    /// the tree exactly once instead of doing a separate full-tree ignore pass.
    pub(crate) fn from_worktree_base(root: &Path) -> Result<Self> {
        let mut matcher = Self::default();
        if !read_core_excludes_file(root, &mut matcher.patterns) {
            read_default_global_excludes_file(&mut matcher.patterns);
        }
        read_ignore_patterns(
            root.join(".git").join("info").join("exclude"),
            &mut matcher.patterns,
            &[],
            b".git/info/exclude",
        );
        matcher.rebuild_buckets();
        Ok(matcher)
    }

    pub(crate) fn from_worktree_root(root: &Path) -> Result<Self> {
        let mut matcher = Self::default();
        if !read_core_excludes_file(root, &mut matcher.patterns) {
            read_default_global_excludes_file(&mut matcher.patterns);
        }
        read_ignore_patterns(
            root.join(".git").join("info").join("exclude"),
            &mut matcher.patterns,
            &[],
            b".git/info/exclude",
        );
        matcher.rebuild_buckets();
        collect_per_directory_patterns_into_matcher(
            root,
            root,
            &[String::from(".gitignore")],
            &mut matcher,
        )?;
        Ok(matcher)
    }

    pub(crate) fn extend_patterns(&mut self, patterns: &[Vec<u8>]) {
        for pattern in patterns {
            self.push_raw_pattern(pattern, &[], &[], 0);
        }
    }

    fn extend_per_directory_patterns(&mut self, root: &Path, names: &[String]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        collect_per_directory_patterns_into_matcher(root, root, names, self)?;
        Ok(())
    }

    pub(crate) fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        self.is_ignored_profiled(path, is_dir, None)
    }

    fn match_for(&self, path: &[u8], is_dir: bool) -> Option<&IgnorePattern> {
        self.match_index_for(path, is_dir, None)
            .and_then(|index| self.patterns.get(index))
    }

    fn is_ignored_profiled(
        &self,
        path: &[u8],
        is_dir: bool,
        mut profile: Option<&mut StatusProfileCounters>,
    ) -> bool {
        if let Some(profile) = profile.as_deref_mut() {
            profile.ignore_checks += 1;
        }
        self.match_index_for(path, is_dir, profile)
            .is_some_and(|index| !self.patterns[index].negated)
    }

    fn match_index_for(
        &self,
        path: &[u8],
        is_dir: bool,
        mut profile: Option<&mut StatusProfileCounters>,
    ) -> Option<usize> {
        let basename = path_basename(path);
        let mut best = None;
        if let Some(indices) = self.buckets.literal_basename.get(basename) {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        if let Some(indices) = self.buckets.literal_path_basename.get(basename) {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        if let Some(indices) = self.buckets.path_suffix_basename.get(basename) {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        if let Some(indices) = self.buckets.glob_path_literal_basename.get(basename) {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        self.match_final_component_candidates(
            &self.buckets.glob_path_suffix_basename,
            MatchKind::Suffix,
            basename,
            path,
            basename,
            is_dir,
            &mut best,
            &mut profile,
        );
        self.match_final_component_candidates(
            &self.buckets.glob_path_prefix_basename,
            MatchKind::Prefix,
            basename,
            path,
            basename,
            is_dir,
            &mut best,
            &mut profile,
        );
        visit_directory_match_components(path, is_dir, |component| {
            if let Some(indices) = self.buckets.directory_literal_basename.get(component) {
                self.match_bucket_candidates(
                    indices,
                    path,
                    basename,
                    is_dir,
                    &mut best,
                    &mut profile,
                );
            }
            if let Some(indices) = self.buckets.directory_literal_path_basename.get(component) {
                self.match_bucket_candidates(
                    indices,
                    path,
                    basename,
                    is_dir,
                    &mut best,
                    &mut profile,
                );
            }
            if let Some(indices) = self.buckets.directory_path_suffix_basename.get(component) {
                self.match_bucket_candidates(
                    indices,
                    path,
                    basename,
                    is_dir,
                    &mut best,
                    &mut profile,
                );
            }
            if let Some(indices) = self.buckets.glob_directory_literal_basename.get(component) {
                self.match_bucket_candidates(
                    indices,
                    path,
                    basename,
                    is_dir,
                    &mut best,
                    &mut profile,
                );
            }
            self.match_final_component_candidates(
                &self.buckets.glob_directory_suffix_basename,
                MatchKind::Suffix,
                component,
                path,
                basename,
                is_dir,
                &mut best,
                &mut profile,
            );
            self.match_final_component_candidates(
                &self.buckets.glob_directory_prefix_basename,
                MatchKind::Prefix,
                component,
                path,
                basename,
                is_dir,
                &mut best,
                &mut profile,
            );
        });
        if let Some(last) = basename.last()
            && let Some(indices) = self.buckets.suffix_basename.get(last)
        {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        if let Some(first) = basename.first()
            && let Some(indices) = self.buckets.prefix_basename.get(first)
        {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        self.match_bucket_candidates(
            &self.buckets.other,
            path,
            basename,
            is_dir,
            &mut best,
            &mut profile,
        );
        best
    }

    fn match_bucket_candidates(
        &self,
        indices: &[usize],
        path: &[u8],
        basename: &[u8],
        is_dir: bool,
        best: &mut Option<usize>,
        profile: &mut Option<&mut StatusProfileCounters>,
    ) {
        for &index in indices.iter().rev() {
            if best.is_some_and(|best| index <= best) {
                break;
            }
            let pattern = &self.patterns[index];
            if !pattern.base_matches(path) {
                continue;
            }
            if !pattern.glob_literal_prefix_matches(path, basename, is_dir) {
                continue;
            }
            if let Some(profile) = profile.as_deref_mut() {
                profile.ignore_pattern_tests += 1;
                if pattern.match_kind == MatchKind::Glob {
                    profile.ignore_glob_fallback_tests += 1;
                }
            }
            if pattern.matches_with_basename(path, basename, is_dir) {
                *best = Some(index);
                break;
            }
        }
    }

    fn match_final_component_candidates(
        &self,
        indices: &[usize],
        kind: MatchKind,
        component: &[u8],
        path: &[u8],
        basename: &[u8],
        is_dir: bool,
        best: &mut Option<usize>,
        profile: &mut Option<&mut StatusProfileCounters>,
    ) {
        for &index in indices.iter().rev() {
            if best.is_some_and(|best| index <= best) {
                break;
            }
            let pattern = &self.patterns[index];
            if !pattern.base_matches(path) {
                continue;
            }
            let final_component = path_basename(&pattern.pattern);
            let candidate = match kind {
                MatchKind::Suffix => component.ends_with(&final_component[1..]),
                MatchKind::Prefix => {
                    component.starts_with(&final_component[..final_component.len() - 1])
                }
                _ => false,
            };
            if !candidate {
                continue;
            }
            if !pattern.glob_literal_prefix_matches(path, basename, is_dir) {
                continue;
            }
            if let Some(profile) = profile.as_deref_mut() {
                profile.ignore_pattern_tests += 1;
                if pattern.match_kind == MatchKind::Glob {
                    profile.ignore_glob_fallback_tests += 1;
                }
            }
            if pattern.matches_with_basename(path, basename, is_dir) {
                *best = Some(index);
                break;
            }
        }
    }

    fn push_pattern(&mut self, pattern: IgnorePattern) {
        let index = self.patterns.len();
        self.buckets.push(index, &pattern);
        self.patterns.push(pattern);
    }

    pub(crate) fn push_raw_pattern(
        &mut self,
        raw: &[u8],
        base: &[u8],
        source: &[u8],
        line_number: usize,
    ) {
        if let Some(pattern) = parse_ignore_pattern(raw, base, source, line_number) {
            self.push_pattern(pattern);
        }
    }

    fn truncate(&mut self, len: usize) {
        if self.patterns.len() == len {
            return;
        }
        self.patterns.truncate(len);
        self.buckets.truncate(len);
    }

    fn rebuild_buckets(&mut self) {
        let mut buckets = IgnorePatternBuckets::default();
        for (index, pattern) in self.patterns.iter().enumerate() {
            buckets.push(index, pattern);
        }
        self.buckets = buckets;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untracked_glob_prefix_stops_at_backslash_escape() {
        assert_eq!(literal_prefix_before_glob(br"dir/\*.rs"), b"dir/");
        assert_eq!(
            literal_prefix_before_glob(br"dir/plain.rs"),
            b"dir/plain.rs"
        );
    }
}
