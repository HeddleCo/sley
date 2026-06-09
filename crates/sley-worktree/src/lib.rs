use sley_config::GitConfig;
use sley_core::{BString, GitError, ObjectFormat, ObjectId, RepoPath, Result};
use sley_index::{CacheTree, Index, IndexEntry, Stage};
use sley_object::TreeEntries;
use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntry, tree_entry_object_type};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry, branch_ref_name};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;
use std::{env, fs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeStatus {
    Clean,
    Modified(RepoPath),
    Added(RepoPath),
    Deleted(RepoPath),
    Untracked(RepoPath),
}

pub trait WorktreeScanner {
    fn status(&self) -> Result<Vec<WorktreeStatus>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseCheckout {
    pub patterns: Vec<Vec<u8>>,
    pub sparse_index: bool,
}

/// Selects how the patterns in a [`SparseCheckout`] are interpreted when
/// deciding which index paths are "in cone" (kept in the worktree).
///
/// * [`SparseCheckoutMode::Full`] interprets the patterns exactly like
///   `.gitignore` lines (full pattern matching, including `*`, `?`, `**`,
///   character classes, anchoring with a leading `/`, directory-only `/`
///   suffixes, and `!` negation). A path is *included* when the last pattern
///   that matches it is not negated. This mirrors upstream Git's non-cone
///   `core.sparseCheckout` behaviour.
/// * [`SparseCheckoutMode::Cone`] interprets the patterns as the restricted
///   directory-prefix form Git emits for `core.sparseCheckoutCone`: a literal
///   `/*` (top-level files), the recursive-parent guard `!/*/`, and anchored
///   directory patterns such as `/dir/` (everything under `dir/`) plus the
///   parent guards `/dir/*` and `!/dir/*/`. Matching is purely prefix based,
///   so glob metacharacters are treated literally.
/// * [`SparseCheckoutMode::Auto`] inspects the patterns and uses cone matching
///   when every pattern fits the cone grammar above, otherwise full matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SparseCheckoutMode {
    #[default]
    Auto,
    Full,
    Cone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplySparseResult {
    /// Paths whose worktree file was (re)materialized because they are in cone.
    pub materialized: Vec<Vec<u8>>,
    /// Paths that were taken out of the worktree because they are out of cone;
    /// their index entry now has the skip-worktree bit set.
    pub skipped: Vec<Vec<u8>>,
    /// Out-of-cone paths whose worktree file was *not* up to date with the index
    /// and was therefore left in place (and its skip-worktree bit left clear),
    /// matching git's data-loss-avoiding behavior. The caller surfaces these as
    /// git's "The following paths are not up to date …" warning. Sorted by path.
    pub not_up_to_date: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateIndexResult {
    pub entries: usize,
    pub updated: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheInfoEntry {
    pub mode: u32,
    pub oid: ObjectId,
    pub path: Vec<u8>,
    pub stage: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexInfoRecord {
    Add(CacheInfoEntry),
    Remove { path: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateIndexOptions {
    pub add: bool,
    pub remove: bool,
    pub force_remove: bool,
    pub chmod: Option<bool>,
    pub info_only: bool,
    pub ignore_skip_worktree_entries: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WriteTreeOptions {
    pub missing_ok: bool,
    pub prefix: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortStatusEntry {
    pub index: u8,
    pub worktree: u8,
    pub path: Vec<u8>,
    pub head_mode: Option<u32>,
    pub index_mode: Option<u32>,
    pub worktree_mode: Option<u32>,
    pub head_oid: Option<ObjectId>,
    pub index_oid: Option<ObjectId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusUntrackedMode {
    #[default]
    All,
    Normal,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShortStatusOptions {
    pub include_ignored: bool,
    pub untracked_mode: StatusUntrackedMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutResult {
    pub branch: String,
    pub oid: ObjectId,
    pub files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreResult {
    pub restored: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveResult {
    pub removed: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveResult {
    pub source: Vec<u8>,
    pub destination: Vec<u8>,
    pub skipped: bool,
    pub fatal: Option<String>,
    pub details: Vec<MoveDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveDetail {
    pub source: Vec<u8>,
    pub destination: Vec<u8>,
    pub skipped: bool,
}

pub fn repository_index_path(git_dir: impl AsRef<Path>) -> PathBuf {
    env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.as_ref().join("index"))
}

pub fn read_repository_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Option<Index>> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(None);
    }
    Ok(Some(Index::parse(&fs::read(index_path)?, format)?))
}

/// Resolve the working-tree root for a repository identified by its git
/// directory, returning `Ok(None)` for a bare repository.
///
/// This is the repository-intrinsic worktree resolution (it does *not* consult
/// `GIT_WORK_TREE`/`GIT_DIR` or CLI overrides — those are the caller's job):
///
/// 1. a `core.worktree` setting in `<git_dir>/config` (absolute, or relative to
///    the git directory), canonicalised;
/// 2. otherwise, for a linked worktree (a git directory that has both a
///    `commondir` and a `gitdir` administrative file), the directory containing
///    the worktree's `.git` link, canonicalised;
/// 3. otherwise, when the git directory is a `.git` directory, its parent (the
///    ordinary non-bare layout) — returned verbatim, not canonicalised;
/// 4. otherwise the repository is bare and `Ok(None)` is returned.
///
/// `Ok(None)` means specifically "bare" (case 4). A [`GitError::Io`] is
/// returned if a path that should exist cannot be canonicalised, and a
/// [`GitError::InvalidPath`] if a `.git` directory has no parent (a malformed
/// layout).
pub fn worktree_root_for_git_dir(git_dir: &Path) -> Result<Option<PathBuf>> {
    if let Ok(config) = GitConfig::read(git_dir.join("config"))
        && let Some(worktree) = config.get("core", None, "worktree")
    {
        let worktree = PathBuf::from(worktree);
        let worktree = if worktree.is_absolute() {
            worktree
        } else {
            git_dir.join(worktree)
        };
        return fs::canonicalize(worktree)
            .map(Some)
            .map_err(|err| GitError::Io(err.to_string()));
    }
    if git_dir.join("commondir").is_file() {
        let gitdir_file = git_dir.join("gitdir");
        if gitdir_file.is_file() {
            let value = fs::read_to_string(&gitdir_file)?;
            let worktree_git_file = resolve_worktree_admin_path(git_dir, value.trim());
            if let Some(worktree) = worktree_git_file.parent() {
                return fs::canonicalize(worktree)
                    .map(Some)
                    .map_err(|err| GitError::Io(err.to_string()));
            }
        }
    }
    if git_dir.file_name().and_then(|name| name.to_str()) != Some(".git") {
        return Ok(None);
    }
    git_dir
        .parent()
        .map(Path::to_path_buf)
        .map(Some)
        .ok_or_else(|| GitError::InvalidPath("git dir has no parent worktree".into()))
}

/// Resolve a path read from a git-directory administrative file (e.g. the
/// `gitdir` link of a linked worktree): absolute paths are kept as-is, relative
/// paths are joined onto the administrative directory.
fn resolve_worktree_admin_path(admin_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        admin_dir.join(path)
    }
}

/// Whether the repository at `git_dir` is shallow — i.e. it has a `shallow`
/// file recording grafted commit boundaries (`git clone --depth`).
pub fn is_shallow_repository(git_dir: &Path) -> bool {
    git_dir.join("shallow").exists()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveOptions {
    pub recursive: bool,
    pub cached: bool,
    pub force: bool,
    pub dry_run: bool,
    pub ignore_unmatch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveOptions {
    pub force: bool,
    pub dry_run: bool,
    pub skip_errors: bool,
}

impl ShortStatusEntry {
    pub fn line(&self) -> String {
        format!(
            "{}{} {}",
            self.index as char,
            self.worktree as char,
            String::from_utf8_lossy(&self.path)
        )
    }
}

pub fn add_paths_to_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<UpdateIndexResult> {
    update_index_paths(
        worktree_root,
        git_dir,
        format,
        paths,
        UpdateIndexOptions {
            add: true,
            remove: false,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
        },
    )
}

pub fn update_index_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
) -> Result<UpdateIndexResult> {
    update_index_paths_impl(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        paths,
        options,
        None,
    )
}

/// Like [`add_paths_to_index`], but runs the configured content filters
/// (`core.autocrlf`/`text`/`eol` EOL conversion and `filter.<name>.clean`
/// drivers) on each file's contents before hashing it into the object store.
///
/// `config` is the repository config used to resolve the filters; pass the
/// parsed `<git_dir>/config` (the orchestrator typically already has this).
pub fn add_paths_to_index_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    config: &GitConfig,
) -> Result<UpdateIndexResult> {
    update_index_paths_filtered(
        worktree_root,
        git_dir,
        format,
        paths,
        UpdateIndexOptions {
            add: true,
            remove: false,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
        },
        config,
    )
}

/// Like [`update_index_paths`], but applies the clean-side content filters (see
/// [`apply_clean_filter`]) to file contents before they are hashed/written.
pub fn update_index_paths_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
    config: &GitConfig,
) -> Result<UpdateIndexResult> {
    update_index_paths_impl(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        paths,
        options,
        Some(config),
    )
}

fn update_index_paths_impl(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
    clean_config: Option<&GitConfig>,
) -> Result<UpdateIndexResult> {
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let mut odb = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut updated = Vec::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if options.force_remove {
            index.entries.retain(|existing| existing.path != git_path);
            continue;
        }
        if let Some(existing) = index
            .entries
            .iter()
            .find(|existing| existing.path == git_path)
            && index_entry_skip_worktree(existing)
        {
            if options.remove && !options.ignore_skip_worktree_entries {
                index.entries.retain(|existing| existing.path != git_path);
            }
            continue;
        }
        if !absolute.exists() {
            if options.remove {
                index.entries.retain(|existing| existing.path != git_path);
                continue;
            }
            print_update_index_path_error(&git_path, "does not exist and --remove not passed");
            return Err(GitError::Exit(128));
        }
        if !options.add
            && !index
                .entries
                .iter()
                .any(|existing| existing.path == git_path)
        {
            print_update_index_path_error(
                &git_path,
                "cannot add to the index - missing --add option?",
            );
            return Err(GitError::Exit(128));
        }
        let body = fs::read(&absolute)?;
        let body = match clean_config {
            Some(config) => apply_clean_filter(worktree_root, git_dir, config, &git_path, &body)?,
            None => body,
        };
        let object = EncodedObject::new(ObjectType::Blob, body);
        let oid = if options.info_only {
            object.object_id(format)?
        } else {
            odb.write_object(object)?
        };
        let metadata = fs::metadata(&absolute)?;
        let mut entry = index_entry_from_metadata(git_path.clone(), oid, &metadata);
        if let Some(executable) = options.chmod {
            entry.mode = if executable { 0o100755 } else { 0o100644 };
        }
        index.entries.retain(|existing| existing.path != git_path);
        index.entries.push(entry);
        updated.push(oid);
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    normalize_index_version_for_extended_flags(&mut index);
    index.extensions = index_extensions_without_cache_tree(&index.extensions);
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated,
    })
}

pub fn refresh_index_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    quiet: bool,
    ignore_missing: bool,
    really_refresh: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(UpdateIndexResult {
            entries: 0,
            updated: Vec::new(),
        });
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let selected_paths = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    let selected_paths = selected_paths.into_iter().collect::<BTreeSet<_>>();
    let mut needs_update = false;
    for entry in &mut index.entries {
        if index_entry_stage(entry) != 0 {
            continue;
        }
        let selected_for_update =
            !selected_paths.is_empty() && selected_paths.contains(entry.path.as_bytes());
        if entry.flags & INDEX_FLAG_ASSUME_UNCHANGED != 0 {
            if !really_refresh {
                continue;
            }
            entry.flags &= !INDEX_FLAG_ASSUME_UNCHANGED;
        }
        let absolute = worktree_root.join(repo_path_to_os_path(entry.path.as_bytes())?);
        let Ok(metadata) = fs::metadata(&absolute) else {
            if ignore_missing {
                continue;
            }
            if !quiet {
                print_update_index_needs_update(entry.path.as_bytes());
            }
            needs_update = true;
            continue;
        };
        if !metadata.is_file() {
            if !quiet {
                print_update_index_needs_update(entry.path.as_bytes());
            }
            needs_update = true;
            continue;
        }
        let body = fs::read(&absolute)?;
        let object = EncodedObject::new(ObjectType::Blob, body);
        let oid = object.object_id(format)?;
        if oid != entry.oid || file_mode(&metadata) != entry.mode {
            if !quiet {
                print_update_index_needs_update(entry.path.as_bytes());
            }
            needs_update = true;
            if selected_for_update {
                *entry = index_entry_from_metadata(entry.path.clone(), oid, &metadata);
            }
            continue;
        }
        *entry = index_entry_from_metadata(entry.path.clone(), oid, &metadata);
    }
    fs::write(&index_path, index.write(format)?)?;
    if needs_update && !quiet {
        return Err(GitError::Exit(1));
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn update_index_again(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(UpdateIndexResult {
            entries: 0,
            updated: Vec::new(),
        });
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    let selected_paths = selected_git_paths(worktree_root, paths)?;
    let mut again_paths = Vec::new();
    for entry in &index.entries {
        if index_entry_stage(entry) != 0 {
            continue;
        }
        if !selected_paths.is_empty() && !git_path_selected(entry.path.as_bytes(), &selected_paths)
        {
            continue;
        }
        let differs_from_head = match head_entries.get(entry.path.as_bytes()) {
            Some(head_entry) => head_entry.oid != entry.oid || head_entry.mode != entry.mode,
            None => true,
        };
        if differs_from_head {
            again_paths.push(worktree_root.join(repo_path_to_os_path(entry.path.as_bytes())?));
        }
    }
    if again_paths.is_empty() {
        return Ok(UpdateIndexResult {
            entries: index.entries.len(),
            updated: Vec::new(),
        });
    }
    update_index_paths(worktree_root, git_dir, format, &again_paths, options)
}

pub fn set_index_assume_unchanged_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    assume_unchanged: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let selected_paths = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    for path in selected_paths {
        if let Some(entry) = index.entries.iter_mut().find(|entry| entry.path == path) {
            if assume_unchanged {
                entry.flags |= INDEX_FLAG_ASSUME_UNCHANGED;
            } else {
                entry.flags &= !INDEX_FLAG_ASSUME_UNCHANGED;
            }
        }
    }
    normalize_index_version_for_extended_flags(&mut index);
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

fn selected_git_paths(worktree_root: &Path, paths: &[PathBuf]) -> Result<BTreeSet<Vec<u8>>> {
    paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect()
}

fn git_path_selected(path: &[u8], selected_paths: &BTreeSet<Vec<u8>>) -> bool {
    selected_paths
        .iter()
        .any(|selected| path == selected || index_entry_is_under_path(path, selected))
}

pub fn set_index_skip_worktree_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    skip_worktree: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let selected_paths = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    for path in selected_paths {
        if let Some(entry) = index.entries.iter_mut().find(|entry| entry.path == path) {
            if skip_worktree {
                entry.flags |= INDEX_FLAG_EXTENDED;
                entry.flags_extended |= INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
            } else {
                entry.flags_extended &= !INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
                if entry.flags_extended == 0 {
                    entry.flags &= !INDEX_FLAG_EXTENDED;
                }
            }
        }
    }
    normalize_index_version_for_extended_flags(&mut index);
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn set_index_fsmonitor_valid_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    _fsmonitor_valid: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let selected_paths = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    for path in selected_paths {
        if !index.entries.iter().any(|entry| entry.path == path) {
            eprintln!(
                "fatal: Unable to mark file {}",
                String::from_utf8_lossy(&path)
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn set_index_version(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    version: u32,
) -> Result<UpdateIndexResult> {
    if !matches!(version, 2..=4) {
        return Err(GitError::Unsupported(format!(
            "update-index currently supports --index-version 2, 3, or 4, got {version}"
        )));
    }
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    index.version = version;
    normalize_index_version_for_extended_flags(&mut index);
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn force_write_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    normalize_index_version_for_extended_flags(&mut index);
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

fn index_extensions_without_cache_tree(extensions: &[u8]) -> Vec<u8> {
    let mut offset = 0;
    let mut filtered = Vec::new();
    while offset < extensions.len() {
        if extensions.len().saturating_sub(offset) < 8 {
            return Vec::new();
        }
        let signature = &extensions[offset..offset + 4];
        let size = u32::from_be_bytes([
            extensions[offset + 4],
            extensions[offset + 5],
            extensions[offset + 6],
            extensions[offset + 7],
        ]) as usize;
        let end = offset + 8 + size;
        if end > extensions.len() {
            return Vec::new();
        }
        if signature != b"TREE" {
            filtered.extend_from_slice(&extensions[offset..end]);
        }
        offset = end;
    }
    filtered
}

pub fn update_index_cacheinfo(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    entries: &[CacheInfoEntry],
    add: bool,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let mut updated = Vec::new();
    for cacheinfo in entries {
        if !add
            && !index
                .entries
                .iter()
                .any(|existing| existing.path == cacheinfo.path)
        {
            let path = String::from_utf8_lossy(&cacheinfo.path);
            eprintln!("error: {path}: cannot add to the index - missing --add option?");
            eprintln!("fatal: git update-index: --cacheinfo cannot add {path}");
            return Err(GitError::Exit(128));
        }
        let flags = index_flags(cacheinfo.path.len(), cacheinfo.stage);
        let entry = IndexEntry {
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
            mtime_seconds: 0,
            mtime_nanoseconds: 0,
            dev: 0,
            ino: 0,
            mode: cacheinfo.mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid: cacheinfo.oid,
            flags,
            flags_extended: 0,
            path: BString::from(cacheinfo.path.as_slice()),
        };
        index.entries.retain(|existing| {
            existing.path != cacheinfo.path || index_entry_stage(existing) != cacheinfo.stage
        });
        index.entries.push(entry);
        updated.push(cacheinfo.oid);
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated,
    })
}

pub fn update_index_index_info(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    records: &[IndexInfoRecord],
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let mut updated = Vec::new();
    for record in records {
        match record {
            IndexInfoRecord::Remove { path } => {
                index.entries.retain(|existing| existing.path != *path);
            }
            IndexInfoRecord::Add(cacheinfo) => {
                let flags = index_flags(cacheinfo.path.len(), cacheinfo.stage);
                let entry = IndexEntry {
                    ctime_seconds: 0,
                    ctime_nanoseconds: 0,
                    mtime_seconds: 0,
                    mtime_nanoseconds: 0,
                    dev: 0,
                    ino: 0,
                    mode: cacheinfo.mode,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    oid: cacheinfo.oid,
                    flags,
                    flags_extended: 0,
                    path: BString::from(cacheinfo.path.as_slice()),
                };
                if cacheinfo.stage == 0 {
                    index
                        .entries
                        .retain(|existing| existing.path != cacheinfo.path);
                } else {
                    index.entries.retain(|existing| {
                        existing.path != cacheinfo.path
                            || index_entry_stage(existing) != cacheinfo.stage
                    });
                }
                index.entries.push(entry);
                updated.push(cacheinfo.oid);
            }
        }
    }
    index.entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
    });
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated,
    })
}

fn index_flags(path_len: usize, stage: u16) -> u16 {
    ((stage & 0x3) << 12) | ((path_len.min(0xfff) as u16) & 0x0fff)
}

const INDEX_FLAG_ASSUME_UNCHANGED: u16 = 0x8000;
const INDEX_FLAG_EXTENDED: u16 = 0x4000;
const INDEX_EXTENDED_FLAG_SKIP_WORKTREE: u16 = 0x4000;

fn normalize_index_version_for_extended_flags(index: &mut Index) {
    let has_extended_flags = index
        .entries
        .iter()
        .any(|entry| entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0);
    if has_extended_flags && index.version < 3 {
        index.version = 3;
    } else if !has_extended_flags && index.version == 3 {
        index.version = 2;
    }
}

fn index_entry_stage(entry: &IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

fn index_entry_skip_worktree(entry: &IndexEntry) -> bool {
    entry.flags & INDEX_FLAG_EXTENDED != 0
        && entry.flags_extended & INDEX_EXTENDED_FLAG_SKIP_WORKTREE != 0
}

fn print_update_index_path_error(path: &[u8], message: &str) {
    let path = String::from_utf8_lossy(path);
    eprintln!("error: {path}: {message}");
    eprintln!("fatal: Unable to process path {path}");
}

fn print_update_index_needs_update(path: &[u8]) {
    let path = String::from_utf8_lossy(path);
    println!("{path}: needs update");
}

pub fn write_tree_from_index(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<ObjectId> {
    write_tree_from_index_with_options(git_dir, format, WriteTreeOptions::default())
}

pub fn write_tree_from_index_with_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: WriteTreeOptions,
) -> Result<ObjectId> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    let entries = write_tree_entries_for_prefix(&index.entries, options.prefix.as_deref())?;
    let mut root = TreeNode::default();
    let odb = FileObjectDatabase::from_git_dir(git_dir, format);
    if !options.missing_ok {
        let mut missing = false;
        for entry in &entries {
            if !odb.contains(&entry.oid)? {
                eprintln!(
                    "error: invalid object {:o} {} for '{}'",
                    entry.mode,
                    entry.oid,
                    String::from_utf8_lossy(entry.path.as_bytes())
                );
                missing = true;
            }
        }
        if missing {
            eprintln!("fatal: git-write-tree: error building trees");
            return Err(GitError::Exit(128));
        }
    }
    for entry in &entries {
        root.insert(entry)?;
    }
    let mut odb = FileObjectDatabase::from_git_dir(git_dir, format);
    write_tree_node(&root, &mut odb)
}

fn write_tree_entries_for_prefix(
    entries: &[IndexEntry],
    prefix: Option<&[u8]>,
) -> Result<Vec<IndexEntry>> {
    let Some(prefix) = prefix else {
        return Ok(entries.to_vec());
    };
    let trimmed_len = prefix
        .iter()
        .rposition(|byte| *byte != b'/')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let trimmed = &prefix[..trimmed_len];
    if trimmed.is_empty() {
        return Ok(entries.to_vec());
    }
    let mut prefixed = Vec::new();
    for entry in entries {
        let Some(remainder) = entry.path.as_bytes().strip_prefix(trimmed) else {
            continue;
        };
        let Some(stripped) = remainder.strip_prefix(b"/") else {
            continue;
        };
        if stripped.is_empty() {
            continue;
        }
        let mut entry = entry.clone();
        entry.path = BString::from(stripped);
        prefixed.push(entry);
    }
    if prefixed.is_empty() {
        eprintln!(
            "fatal: git-write-tree: prefix {} not found",
            String::from_utf8_lossy(prefix)
        );
        return Err(GitError::Exit(128));
    }
    Ok(prefixed)
}

pub fn short_status(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<ShortStatusEntry>> {
    short_status_with_options(
        worktree_root,
        git_dir,
        format,
        ShortStatusOptions::default(),
    )
}

pub fn short_status_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: ShortStatusOptions,
) -> Result<Vec<ShortStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    // Parse the index once: the tracked map drives status comparisons and the
    // stat cache lets the worktree walk skip re-hashing files whose stat proves
    // they are unchanged since staging (git's racy-git shortcut).
    let (index, stat_cache, head_matches_index) =
        read_index_entries_with_stat_cache(git_dir, format, &db)?;
    let head = if head_matches_index {
        index.clone()
    } else {
        head_tree_entries(git_dir, format, &db)?
    };
    let tracked_paths = if options.untracked_mode == StatusUntrackedMode::None {
        Some(index.keys().cloned().collect::<BTreeSet<_>>())
    } else {
        None
    };
    let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
    let worktree = worktree_entries_with_stat_cache(
        worktree_root,
        git_dir,
        format,
        Some(&stat_cache),
        tracked_paths.as_ref(),
        Some(&mut ignores),
    )?;
    let mut paths = BTreeSet::new();
    paths.extend(head.keys().cloned());
    paths.extend(index.keys().cloned());
    paths.extend(
        worktree
            .keys()
            .filter(|path| index.contains_key(*path))
            .cloned(),
    );

    let mut entries = Vec::new();
    for path in paths {
        let head_entry = head.get(&path);
        let index_entry = index.get(&path);
        let worktree_entry = worktree.get(&path);
        if head_entry.is_none()
            && index_entry.is_none()
            && worktree_entry.is_some()
            && ignores.is_ignored(&path, false)
        {
            continue;
        }
        let (index_code, worktree_code) =
            if head_entry.is_none() && index_entry.is_none() && worktree_entry.is_some() {
                (b'?', b'?')
            } else {
                let index_code = match (head_entry, index_entry) {
                    (None, Some(_)) => b'A',
                    (Some(_), None) => b'D',
                    (Some(left), Some(right)) if left != right => b'M',
                    _ => b' ',
                };
                let worktree_code = match (index_entry, worktree_entry) {
                    (None, Some(_)) => b'?',
                    (Some(_), None) => b'D',
                    (Some(left), Some(right)) if left != right => b'M',
                    _ => b' ',
                };
                (index_code, worktree_code)
            };
        if index_code != b' ' || worktree_code != b' ' {
            entries.push(ShortStatusEntry {
                index: index_code,
                worktree: worktree_code,
                path,
                head_mode: head_entry.map(|entry| entry.mode),
                index_mode: index_entry.map(|entry| entry.mode),
                worktree_mode: worktree_entry.map(|entry| entry.mode),
                head_oid: head_entry.map(|entry| entry.oid),
                index_oid: index_entry.map(|entry| entry.oid),
            });
        }
    }
    if options.include_ignored {
        for path in ignored_untracked_paths(worktree_root, git_dir, &index, &ignores, true)? {
            entries.push(ShortStatusEntry {
                index: b'!',
                worktree: b'!',
                path,
                head_mode: None,
                index_mode: None,
                worktree_mode: None,
                head_oid: None,
                index_oid: None,
            });
        }
    }
    let untracked_paths: Vec<Vec<u8>> = match options.untracked_mode {
        StatusUntrackedMode::All => worktree
            .keys()
            .filter(|path| !index.contains_key(*path) && !ignores.is_ignored(path, false))
            .cloned()
            .collect(),
        StatusUntrackedMode::Normal => {
            normal_untracked_paths_from_worktree(&worktree, &index, &ignores)
        }
        StatusUntrackedMode::None => Vec::new(),
    };
    for path in untracked_paths {
        entries.push(ShortStatusEntry {
            index: b'?',
            worktree: b'?',
            path,
            head_mode: None,
            index_mode: None,
            worktree_mode: None,
            head_oid: None,
            index_oid: None,
        });
    }
    entries.sort_by(|left, right| {
        status_sort_category(left)
            .cmp(&status_sort_category(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

fn status_sort_category(entry: &ShortStatusEntry) -> u8 {
    match (entry.index, entry.worktree) {
        (b'?', b'?') => 1,
        (b'!', b'!') => 2,
        _ => 0,
    }
}

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
    /// When set (ls-files `--directory`), untracked files roll up to `parent/`.
    pub rollup_untracked_files_to_directories: bool,
}

pub fn pathspec_is_glob(path: &[u8]) -> bool {
    path.iter().any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

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

fn glob_spec_matches_directory(spec: &UntrackedPathspecFilter, git_path: &[u8]) -> bool {
    spec.is_glob
        && (untracked_wildmatch(&spec.path, git_path)
            || git_path
                .strip_suffix(b"/")
                .is_some_and(|stripped| untracked_wildmatch(&spec.path, stripped)))
}

fn glob_pathspec_may_match_under(pattern: &[u8], dir: &[u8]) -> bool {
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

fn literal_prefix_before_glob(pattern: &[u8]) -> Vec<u8> {
    let mut prefix = Vec::new();
    for &byte in pattern {
        if matches!(byte, b'*' | b'?' | b'[') {
            break;
        }
        prefix.push(byte);
    }
    prefix
}

fn insert_untracked_directory(paths: &mut BTreeSet<Vec<u8>>, git_path: &[u8]) {
    let mut directory = git_path.to_vec();
    if directory.last() != Some(&b'/') {
        directory.push(b'/');
    }
    paths.insert(directory);
}

fn parent_git_path(path: &[u8]) -> Option<&[u8]> {
    let slash = path.iter().rposition(|byte| *byte == b'/')?;
    if slash == 0 {
        return None;
    }
    Some(&path[..slash])
}

/// fnmatch-style glob where `*` and `?` match any byte including `/`.
fn untracked_wildmatch(pattern: &[u8], text: &[u8]) -> bool {
    fn rec(pattern: &[u8], text: &[u8]) -> bool {
        let mut pi = 0;
        let mut ti = 0;
        while pi < pattern.len() {
            match pattern[pi] {
                b'*' => {
                    while pi < pattern.len() && pattern[pi] == b'*' {
                        pi += 1;
                    }
                    if pi == pattern.len() {
                        return true;
                    }
                    let mut k = ti;
                    loop {
                        if rec(&pattern[pi..], &text[k..]) {
                            return true;
                        }
                        if k >= text.len() {
                            return false;
                        }
                        k += 1;
                    }
                }
                b'?' => {
                    if ti >= text.len() {
                        return false;
                    }
                    pi += 1;
                    ti += 1;
                }
                b'[' => match untracked_match_bracket(&pattern[pi..], text[ti]) {
                    UntrackedBracketOutcome::Match(consumed) => {
                        pi += consumed;
                        ti += 1;
                    }
                    UntrackedBracketOutcome::NoMatch => return false,
                    UntrackedBracketOutcome::Malformed => {
                        if ti >= text.len() || text[ti] != b'[' {
                            return false;
                        }
                        pi += 1;
                        ti += 1;
                    }
                },
                b'\\' if pi + 1 < pattern.len() => {
                    if ti >= text.len() || text[ti] != pattern[pi + 1] {
                        return false;
                    }
                    pi += 2;
                    ti += 1;
                }
                literal => {
                    if ti >= text.len() || text[ti] != literal {
                        return false;
                    }
                    pi += 1;
                    ti += 1;
                }
            }
        }
        ti == text.len()
    }
    rec(pattern, text)
}

enum UntrackedBracketOutcome {
    Match(usize),
    NoMatch,
    Malformed,
}

fn untracked_match_bracket(pattern: &[u8], ch: u8) -> UntrackedBracketOutcome {
    let mut i = 1;
    let negate = matches!(pattern.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pattern.len() {
        let c = pattern[i];
        if c == b']' && !first {
            let hit = matched != negate;
            return if hit {
                UntrackedBracketOutcome::Match(i + 1)
            } else {
                UntrackedBracketOutcome::NoMatch
            };
        }
        first = false;
        if i + 2 < pattern.len() && pattern[i + 1] == b'-' && pattern[i + 2] != b']' {
            let lo = c;
            let hi = pattern[i + 2];
            if lo <= ch && ch <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if c == ch {
                matched = true;
            }
            i += 1;
        }
    }
    UntrackedBracketOutcome::Malformed
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
        &worktree, &index, &ignores,
    ))
}

/// Untracked paths for `ls-files --others` (without `--directory`): every
/// untracked file is listed individually, except embedded-repository boundaries
/// which are emitted as `dir/` to match git's non-submodule `.git` handling.
fn ls_files_untracked_paths_from_worktree(
    worktree: &BTreeMap<Vec<u8>, TrackedEntry>,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for (path, entry) in worktree {
        if index.contains_key(path) || ignores.is_ignored(path, false) {
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

pub fn standard_attributes_for_path_from_tree(
    worktree_root: impl AsRef<Path>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    path: &[u8],
    requested: &[Vec<u8>],
    all: bool,
) -> Result<Vec<AttributeCheck>> {
    let mut matcher = AttributeMatcher::default();
    let worktree_root = worktree_root.as_ref();
    if !matcher.read_configured_attributes(worktree_root) {
        matcher.read_default_global_attributes();
    }
    collect_attribute_patterns_from_tree(db, format, tree_oid, Vec::new(), &mut matcher)?;
    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
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
    if !matcher.read_configured_attributes(worktree_root) {
        matcher.read_default_global_attributes();
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    collect_attribute_patterns_from_index(git_dir, format, &db, &mut matcher)?;
    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
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

fn collect_untracked_directory_paths(
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
        if is_worktree_dot_git(root, &path) {
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
        if metadata.is_dir() {
            if is_nested_repository_boundary(&path) {
                insert_untracked_directory(paths, &git_path);
                continue;
            }
            let has_tracked_below = index_has_path_under(index, &git_path);
            let needs_descent = untracked_pathspec_needs_descent(&git_path, &options.pathspecs);
            if has_tracked_below {
                collect_untracked_directory_paths(
                    root, git_dir, &path, index, ignores, options, paths,
                )?;
            } else if needs_descent {
                if options
                    .pathspecs
                    .iter()
                    .any(|spec| glob_spec_matches_directory(spec, &git_path))
                {
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
            if options.rollup_untracked_files_to_directories {
                if let Some(parent) = parent_git_path(&git_path) {
                    insert_untracked_directory(paths, parent);
                } else {
                    paths.insert(git_path);
                }
            } else {
                paths.insert(git_path);
            }
        }
    }
    Ok(())
}

fn index_has_path_under(index: &BTreeMap<Vec<u8>, TrackedEntry>, directory: &[u8]) -> bool {
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
fn normal_untracked_paths_from_worktree(
    worktree: &BTreeMap<Vec<u8>, TrackedEntry>,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for (path, entry) in worktree {
        if index.contains_key(path) || ignores.is_ignored(path, false) {
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

fn untracked_normal_rollup_path(
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
            break;
        }
        if !ignores.is_ignored(&prefix, true) {
            let mut directory = prefix;
            directory.push(b'/');
            return directory;
        }
    }
    file_path.to_vec()
}

fn directory_has_file(
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
        if is_worktree_dot_git(root, &path) {
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
            if is_nested_repository_boundary(&path) {
                continue;
            }
            if directory_has_file(&path, root, git_dir, ignores)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn directory_has_ignored(
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
        if is_worktree_dot_git(root, &path) {
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

fn ignored_untracked_paths(
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

struct IgnoredUntrackedContext<'a> {
    root: &'a Path,
    git_dir: &'a Path,
    index: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &'a IgnoreMatcher,
    directory: bool,
}

fn collect_ignored_untracked_paths(
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
        if is_worktree_dot_git(context.root, &path) {
            continue;
        }
        if is_embedded_git_internals(context.root, &path) {
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
            if is_nested_repository_boundary(&path) {
                continue;
            }
            let ignored = parent_ignored || context.ignores.is_ignored(&git_path, true);
            if ignored && !index_has_path_under(context.index, &git_path) {
                if context.directory {
                    let mut directory_path = git_path;
                    directory_path.push(b'/');
                    paths.insert(directory_path);
                } else {
                    collect_ignored_untracked_paths(context, &path, true, paths)?;
                }
            } else {
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

#[derive(Debug, Default)]
struct IgnoreMatcher {
    patterns: Vec<IgnorePattern>,
}

#[derive(Debug)]
struct IgnorePattern {
    base: Vec<u8>,
    pattern: Vec<u8>,
    original: Vec<u8>,
    source: Vec<u8>,
    line_number: usize,
    negated: bool,
    directory_only: bool,
    anchored: bool,
    has_slash: bool,
    /// How `pattern` should be matched against a slash-free segment. Most
    /// `.gitignore` entries are literals or simple `*.ext` / `prefix*` globs, all
    /// of which match without the allocating wildcard DP engine; only genuinely
    /// complex globs fall through to [`wildcard_path_matches`].
    match_kind: MatchKind,
}

/// Classification of an [`IgnorePattern`] that lets common shapes skip the
/// general wildcard matcher. Every variant matches a *slash-free* segment
/// (basename or path component); patterns containing `/` are always
/// [`MatchKind::Glob`] so they only ever reach the full engine.
#[derive(Debug)]
enum MatchKind {
    /// No metacharacters: matches by byte equality.
    Literal,
    /// `*X` with `X` literal: matches a segment ending in `X`.
    Suffix,
    /// `X*` with `X` literal: matches a segment starting with `X`.
    Prefix,
    /// Anything else: defer to [`wildcard_path_matches`].
    Glob,
}

/// Classify `pattern` for [`MatchKind`]. `*X`/`X*` fast paths require the literal
/// part to be slash-free so that `ends_with`/`starts_with` on a single segment is
/// exactly equivalent to the glob (`*` never crosses `/`).
fn classify_ignore_pattern(pattern: &[u8]) -> MatchKind {
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
    fn from_worktree_base(root: &Path) -> Result<Self> {
        let mut patterns = Vec::new();
        read_ignore_patterns(
            root.join(".git").join("info").join("exclude"),
            &mut patterns,
            &[],
            b".git/info/exclude",
        );
        if !read_core_excludes_file(root, &mut patterns) {
            read_default_global_excludes_file(&mut patterns);
        }
        Ok(Self { patterns })
    }

    fn fold_dir_ignore_patterns(&mut self, root: &Path, dir: &Path) -> Result<()> {
        let relative = dir.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", dir.display()))
        })?;
        let base = git_path_bytes(relative)?;
        let mut source = base.clone();
        if !source.is_empty() {
            source.push(b'/');
        }
        source.extend_from_slice(b".gitignore");
        read_ignore_patterns(dir.join(".gitignore"), &mut self.patterns, &base, &source);
        Ok(())
    }

    fn from_worktree_root(root: &Path) -> Result<Self> {
        let mut patterns = Vec::new();
        read_ignore_patterns(
            root.join(".git").join("info").join("exclude"),
            &mut patterns,
            &[],
            b".git/info/exclude",
        );
        if !read_core_excludes_file(root, &mut patterns) {
            read_default_global_excludes_file(&mut patterns);
        }
        collect_per_directory_patterns(root, root, &[String::from(".gitignore")], &mut patterns)?;
        Ok(Self { patterns })
    }

    fn extend_patterns(&mut self, patterns: &[Vec<u8>]) {
        for pattern in patterns {
            push_ignore_pattern(&mut self.patterns, pattern, &[], &[], 0);
        }
    }

    fn extend_per_directory_patterns(&mut self, root: &Path, names: &[String]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        collect_per_directory_patterns(root, root, names, &mut self.patterns)
    }

    fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        let mut ignored = false;
        for pattern in &self.patterns {
            if pattern.matches(path, is_dir) {
                ignored = !pattern.negated;
            }
        }
        ignored
    }

    fn match_for(&self, path: &[u8], is_dir: bool) -> Option<&IgnorePattern> {
        let mut matched = None;
        for pattern in &self.patterns {
            if pattern.matches(path, is_dir) {
                matched = Some(pattern);
            }
        }
        matched
    }
}

/// Decides whether a worktree path is included by a [`SparseCheckout`].
///
/// In [`SparseCheckoutMode::Full`] the sparse patterns are compiled with the
/// same `.gitignore` grammar used elsewhere in this crate ([`IgnorePattern`]);
/// a path is *in cone* when the last matching pattern is positive. In
/// [`SparseCheckoutMode::Cone`] the patterns are reduced to a set of recursive
/// directory prefixes plus a flag for whether top-level files are kept, and
/// inclusion is decided by literal prefix containment.
#[derive(Debug)]
enum SparseMatcher {
    Full { patterns: Vec<IgnorePattern> },
    Cone(ConeMatcher),
}

#[derive(Debug, Default)]
struct ConeMatcher {
    /// `true` when files directly at the repository root are in cone (`/*`).
    root_files: bool,
    /// Directory prefixes (without leading or trailing `/`) whose entire
    /// subtree is in cone, e.g. `dir1/dir2`.
    recursive_dirs: Vec<Vec<u8>>,
    /// Parent directories that are in cone only for their direct files
    /// (the `/dir/*` guard Git emits so intermediate directories keep their
    /// own files). Stored without leading or trailing `/`.
    parent_dirs: Vec<Vec<u8>>,
}

impl SparseMatcher {
    fn new(sparse: &SparseCheckout, mode: SparseCheckoutMode) -> Self {
        let resolved = match mode {
            SparseCheckoutMode::Auto => {
                if patterns_are_cone(&sparse.patterns) {
                    SparseCheckoutMode::Cone
                } else {
                    SparseCheckoutMode::Full
                }
            }
            other => other,
        };
        match resolved {
            SparseCheckoutMode::Cone => SparseMatcher::Cone(ConeMatcher::compile(&sparse.patterns)),
            // `Auto` has been resolved above; everything else is full matching.
            _ => {
                let mut patterns = Vec::new();
                for pattern in &sparse.patterns {
                    push_ignore_pattern(&mut patterns, pattern, &[], b"sparse-checkout", 0);
                }
                SparseMatcher::Full { patterns }
            }
        }
    }

    /// Returns `true` when the given file path should be present in the
    /// worktree under this sparse specification.
    fn includes_file(&self, path: &[u8]) -> bool {
        match self {
            SparseMatcher::Full { patterns } => {
                let mut included = false;
                for pattern in patterns {
                    if pattern.matches(path, false) {
                        included = !pattern.negated;
                    }
                }
                included
            }
            SparseMatcher::Cone(cone) => cone.includes_file(path),
        }
    }
}

impl ConeMatcher {
    fn compile(patterns: &[Vec<u8>]) -> Self {
        let mut matcher = ConeMatcher::default();
        for raw in patterns {
            let line = sparse_clean_line(raw);
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            // Negated guards such as `!/*/` and `!/dir/*/` only exist to stop a
            // recursive match from pulling in nested directories; the positive
            // patterns already capture the cone, so we ignore the negations.
            if line.starts_with(b"!") {
                continue;
            }
            if line == b"/*" {
                matcher.root_files = true;
                continue;
            }
            // `/dir/` -> recursive subtree.
            if let Some(rest) = line.strip_prefix(b"/")
                && let Some(dir) = rest.strip_suffix(b"/")
                && !dir.is_empty()
            {
                matcher.recursive_dirs.push(dir.to_vec());
                continue;
            }
            // `/dir/*` -> direct files of `dir` only (parent guard).
            if let Some(rest) = line.strip_prefix(b"/")
                && let Some(dir) = rest.strip_suffix(b"/*")
                && !dir.is_empty()
            {
                matcher.parent_dirs.push(dir.to_vec());
                continue;
            }
        }
        matcher
    }

    fn includes_file(&self, path: &[u8]) -> bool {
        let parent = match path.iter().rposition(|byte| *byte == b'/') {
            Some(index) => &path[..index],
            None => {
                // A path with no slash is a top-level file.
                return self.root_files;
            }
        };
        if self
            .recursive_dirs
            .iter()
            .any(|dir| path_is_under_dir(path, dir))
        {
            return true;
        }
        self.parent_dirs.iter().any(|dir| dir.as_slice() == parent)
    }
}

/// Strips a CR, leading/trailing whitespace, and an optional trailing slash is
/// preserved (cone patterns are slash sensitive) from a raw sparse line.
fn sparse_clean_line(raw: &[u8]) -> &[u8] {
    let line = raw.strip_suffix(b"\r").unwrap_or(raw);
    trim_ascii_whitespace(line)
}

/// Returns `true` when `path` is the directory `dir` itself or lives anywhere
/// beneath it.
fn path_is_under_dir(path: &[u8], dir: &[u8]) -> bool {
    if dir.is_empty() {
        return true;
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.first() == Some(&b'/'))
}

/// Heuristic used by [`SparseCheckoutMode::Auto`]: the pattern set is cone
/// shaped when every (non-comment, non-blank) line is one of the restricted
/// cone forms Git emits.
fn patterns_are_cone(patterns: &[Vec<u8>]) -> bool {
    let mut saw_pattern = false;
    for raw in patterns {
        let line = sparse_clean_line(raw);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        saw_pattern = true;
        let body = line.strip_prefix(b"!").unwrap_or(line);
        let is_cone_shaped = body == b"/*"
            || body == b"/*/"
            || (body.starts_with(b"/")
                && (body.ends_with(b"/") || body.ends_with(b"/*"))
                && !sparse_has_glob_meta(body));
        if !is_cone_shaped {
            return false;
        }
    }
    saw_pattern
}

/// Detects glob metacharacters that disqualify a line from cone interpretation.
/// A single trailing `/*` is allowed by the caller and handled separately.
fn sparse_has_glob_meta(body: &[u8]) -> bool {
    let trimmed = body.strip_suffix(b"/*").unwrap_or(body);
    trimmed
        .iter()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\'))
}

fn read_core_excludes_file(root: &Path, patterns: &mut Vec<IgnorePattern>) -> bool {
    let Ok(config) = GitConfig::read(root.join(".git").join("config")) else {
        return false;
    };
    let Some(value) = config.get("core", None, "excludesFile") else {
        return false;
    };
    let path = expand_core_excludes_file(root, value);
    read_ignore_patterns(path, patterns, &[], value.as_bytes());
    true
}

fn expand_core_excludes_file(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    root.join(path)
}

fn read_default_global_excludes_file(patterns: &mut Vec<IgnorePattern>) {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        && !config_home.is_empty()
    {
        let path = PathBuf::from(config_home).join("git").join("ignore");
        let source = path.to_string_lossy().into_owned();
        read_ignore_patterns(path, patterns, &[], source.as_bytes());
        return;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home)
            .join(".config")
            .join("git")
            .join("ignore");
        let source = path.to_string_lossy().into_owned();
        read_ignore_patterns(path, patterns, &[], source.as_bytes());
    }
}

fn collect_per_directory_patterns(
    root: &Path,
    dir: &Path,
    names: &[String],
    patterns: &mut Vec<IgnorePattern>,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_per_directory_patterns(root, &path, names, patterns)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !names.iter().any(|name| name == file_name) {
            continue;
        }
        let parent = path.parent().unwrap_or(root);
        let relative = parent.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", parent.display()))
        })?;
        let base = git_path_bytes(relative)?;
        let mut source = base.clone();
        if !source.is_empty() {
            source.push(b'/');
        }
        source.extend_from_slice(file_name.as_bytes());
        read_ignore_patterns(&path, patterns, &base, &source);
    }
    Ok(())
}

fn read_ignore_patterns(
    path: impl AsRef<Path>,
    patterns: &mut Vec<IgnorePattern>,
    base: &[u8],
    source: &[u8],
) {
    let Ok(contents) = fs::read(path) else {
        return;
    };
    for (line, raw) in contents.split(|byte| *byte == b'\n').enumerate() {
        push_ignore_pattern(patterns, raw, base, source, line + 1);
    }
}

fn push_ignore_pattern(
    patterns: &mut Vec<IgnorePattern>,
    raw: &[u8],
    base: &[u8],
    source: &[u8],
    line_number: usize,
) {
    let mut line = raw.strip_suffix(b"\r").unwrap_or(raw).to_vec();
    normalize_ignore_trailing_spaces(&mut line);
    let original = line.clone();
    let mut line = line.as_slice();
    if line.is_empty() || line.starts_with(b"#") {
        return;
    }
    let negated = if line.starts_with(b"\\#") || line.starts_with(b"\\!") {
        line = &line[1..];
        false
    } else if let Some(pattern) = line.strip_prefix(b"!") {
        line = pattern;
        true
    } else {
        false
    };
    let directory_only = line.ends_with(b"/");
    let pattern = if directory_only {
        line.strip_suffix(b"/").unwrap_or(line)
    } else {
        line
    };
    let (anchored, pattern) = if let Some(pattern) = pattern.strip_prefix(b"/") {
        (true, pattern)
    } else {
        (false, pattern)
    };
    // A leading `**/` followed by a slash-free segment is, per gitignore,
    // identical to the bare segment ("match in all directories"): `**/Pods` ≡
    // `Pods`, `**/*.jks` ≡ `*.jks`. Collapse it so the pattern matches the
    // basename directly (a literal/suffix compare) instead of paying for the
    // `**` wildcard engine on the full path — verified against `git check-ignore`.
    let pattern = match pattern.strip_prefix(b"**/") {
        Some(rest) if !rest.is_empty() && !rest.contains(&b'/') => rest,
        _ => pattern,
    };
    if pattern.is_empty() {
        return;
    }
    patterns.push(IgnorePattern {
        base: base.to_vec(),
        pattern: pattern.to_vec(),
        original,
        source: source.to_vec(),
        line_number,
        negated,
        directory_only,
        anchored,
        has_slash: pattern.contains(&b'/'),
        match_kind: classify_ignore_pattern(pattern),
    });
}

fn normalize_ignore_trailing_spaces(line: &mut Vec<u8>) {
    while line.last() == Some(&b' ') {
        let space_index = line.len() - 1;
        let backslashes = line[..space_index]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if backslashes % 2 == 1 {
            line.remove(space_index - 1);
            break;
        }
        line.pop();
    }
}

impl IgnorePattern {
    fn to_match(&self) -> IgnoreMatch {
        IgnoreMatch {
            source: self.source.clone(),
            line_number: self.line_number,
            pattern: self.original.clone(),
            ignored: !self.negated,
        }
    }

    fn matches(&self, path: &[u8], is_dir: bool) -> bool {
        let path = if self.base.is_empty() {
            path
        } else {
            let Some(rest) = path
                .strip_prefix(self.base.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
            else {
                return false;
            };
            rest
        };
        if self.directory_only {
            return self.matches_directory(path, is_dir);
        }
        if self.anchored || self.has_slash {
            return self.match_segment(path);
        }
        path.rsplit(|byte| *byte == b'/')
            .next()
            .is_some_and(|basename| self.match_segment(basename))
    }

    fn matches_directory(&self, path: &[u8], is_dir: bool) -> bool {
        if self.anchored || self.has_slash {
            return path == self.pattern
                || path
                    .strip_prefix(self.pattern.as_slice())
                    .and_then(|rest| rest.strip_prefix(b"/"))
                    .is_some();
        }
        path.split(|byte| *byte == b'/')
            .enumerate()
            .any(|(index, component)| {
                self.match_segment(component)
                    && (is_dir || index + 1 < path.split(|byte| *byte == b'/').count())
            })
    }

    /// Match a slash-free `value` (a basename or path component) against this
    /// pattern. Literal and simple `*X`/`X*` patterns resolve with a direct
    /// comparison; only complex globs pay for the allocating wildcard engine.
    fn match_segment(&self, value: &[u8]) -> bool {
        match self.match_kind {
            MatchKind::Literal => self.pattern == value,
            // `*X` ≡ ends_with(X) and `X*` ≡ starts_with(X), but only on a
            // slash-free segment: `*` never crosses `/`, so an anchored `/*.log`
            // applied to a multi-segment path must not match (the slash guard
            // rejects it). Basename/component call sites are slash-free already.
            MatchKind::Suffix => !value.contains(&b'/') && value.ends_with(&self.pattern[1..]),
            MatchKind::Prefix => {
                !value.contains(&b'/') && value.starts_with(&self.pattern[..self.pattern.len() - 1])
            }
            MatchKind::Glob => wildcard_path_matches(&self.pattern, value),
        }
    }
}

thread_local! {
    /// Reused dynamic-programming scratch for [`wildcard_path_matches`]. Flat
    /// `(pattern.len()+1) * (value.len()+1)` grid of memoised results, kept across
    /// calls so the hot ignore/attribute matching loop never reallocates.
    static WILDCARD_MEMO: RefCell<Vec<Option<bool>>> = const { RefCell::new(Vec::new()) };
}

fn wildcard_path_matches(pattern: &[u8], value: &[u8]) -> bool {
    let stride = value.len() + 1;
    let cells = (pattern.len() + 1) * stride;
    WILDCARD_MEMO.with_borrow_mut(|memo| {
        // One reused allocation; clearing then resizing fills the grid with `None`.
        memo.clear();
        memo.resize(cells, None);
        wildcard_path_matches_from(pattern, value, 0, 0, memo, stride)
    })
}

fn wildcard_path_matches_from(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Option<bool>],
    stride: usize,
) -> bool {
    let cell = pattern_index * stride + value_index;
    if let Some(cached) = memo[cell] {
        return cached;
    }
    let matched = if pattern_index == pattern.len() {
        value_index == value.len()
    } else {
        match pattern[pattern_index] {
            b'*' if pattern.get(pattern_index + 1) == Some(&b'*') => wildcard_double_star_matches(
                pattern,
                value,
                pattern_index,
                value_index,
                memo,
                stride,
            ),
            b'*' => {
                if wildcard_path_matches_from(
                    pattern,
                    value,
                    pattern_index + 1,
                    value_index,
                    memo,
                    stride,
                ) {
                    true
                } else {
                    let mut next = value_index;
                    while next < value.len() && value[next] != b'/' {
                        next += 1;
                        if wildcard_path_matches_from(
                            pattern,
                            value,
                            pattern_index + 1,
                            next,
                            memo,
                            stride,
                        ) {
                            return true;
                        }
                    }
                    false
                }
            }
            b'?' => {
                value_index < value.len()
                    && value[value_index] != b'/'
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 1,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
            b'[' => {
                if value_index < value.len() && value[value_index] != b'/' {
                    if let Some((class_matches, next_pattern_index)) =
                        wildcard_class_matches(pattern, pattern_index, value[value_index])
                    {
                        class_matches
                            && wildcard_path_matches_from(
                                pattern,
                                value,
                                next_pattern_index,
                                value_index + 1,
                                memo,
                                stride,
                            )
                    } else {
                        value[value_index] == b'['
                            && wildcard_path_matches_from(
                                pattern,
                                value,
                                pattern_index + 1,
                                value_index + 1,
                                memo,
                                stride,
                            )
                    }
                } else {
                    false
                }
            }
            b'\\' if pattern_index + 1 < pattern.len() => {
                value_index < value.len()
                    && pattern[pattern_index + 1] == value[value_index]
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 2,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
            literal => {
                value_index < value.len()
                    && literal == value[value_index]
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 1,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
        }
    };
    memo[cell] = Some(matched);
    matched
}

fn wildcard_double_star_matches(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Option<bool>],
    stride: usize,
) -> bool {
    let after_stars = pattern_index + 2;
    if pattern.get(after_stars) == Some(&b'/') {
        if wildcard_path_matches_from(pattern, value, after_stars + 1, value_index, memo, stride) {
            return true;
        }
        for next in value_index..value.len() {
            if value[next] == b'/'
                && wildcard_path_matches_from(
                    pattern,
                    value,
                    after_stars + 1,
                    next + 1,
                    memo,
                    stride,
                )
            {
                return true;
            }
        }
        return false;
    }
    for next in value_index..=value.len() {
        if wildcard_path_matches_from(pattern, value, after_stars, next, memo, stride) {
            return true;
        }
    }
    false
}

fn wildcard_class_matches(pattern: &[u8], start: usize, value: u8) -> Option<(bool, usize)> {
    let mut index = start + 1;
    let negated = matches!(pattern.get(index), Some(b'!' | b'^'));
    if negated {
        index += 1;
    }
    let class_start = index;
    let end = pattern[class_start..]
        .iter()
        .position(|byte| *byte == b']')
        .map(|position| class_start + position)?;
    if end == class_start {
        return None;
    }
    let mut matched = false;
    while index < end {
        if index + 2 < end && pattern[index + 1] == b'-' {
            let lower = pattern[index].min(pattern[index + 2]);
            let upper = pattern[index].max(pattern[index + 2]);
            matched |= lower <= value && value <= upper;
            index += 3;
        } else {
            matched |= pattern[index] == value;
            index += 1;
        }
    }
    Some((if negated { !matched } else { matched }, end + 1))
}

#[derive(Debug, Default)]
struct AttributeMatcher {
    patterns: Vec<AttributePattern>,
    attribute_order: BTreeMap<Vec<u8>, usize>,
    macros: BTreeMap<Vec<u8>, Vec<AttributeAssignment>>,
}

#[derive(Debug)]
struct AttributePattern {
    base: Vec<u8>,
    pattern: Vec<u8>,
    anchored: bool,
    has_slash: bool,
    assignments: Vec<AttributeAssignment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttributeAssignment {
    attribute: Vec<u8>,
    state: Option<AttributeState>,
}

impl AttributeMatcher {
    fn from_worktree_root(root: &Path) -> Result<Self> {
        let mut matcher = Self::default();
        if !matcher.read_configured_attributes(root) {
            matcher.read_default_global_attributes();
        }
        collect_attribute_patterns(root, root, &mut matcher)?;
        read_attribute_patterns(
            root.join(".git").join("info").join("attributes"),
            &mut matcher,
            &[],
            b".git/info/attributes",
        );
        Ok(matcher)
    }

    /// Builds only the repository-wide attribute sources — `core.attributesFile`
    /// (or the default global) and `$GIT_DIR/info/attributes` — *without* walking
    /// the worktree for `.gitattributes`. The caller is expected to fold each
    /// directory's `.gitattributes` into the matcher as it descends (see
    /// [`read_dir_attribute_patterns`]), so status/diff read the tree exactly once
    /// instead of doing a separate full-tree attribute pass. Lower-priority sources
    /// are added first, so in-tree patterns added during the walk take precedence —
    /// matching git's lookup order.
    fn from_worktree_base(root: &Path) -> Self {
        let mut matcher = Self::default();
        if !matcher.read_configured_attributes(root) {
            matcher.read_default_global_attributes();
        }
        read_attribute_patterns(
            root.join(".git").join("info").join("attributes"),
            &mut matcher,
            &[],
            b".git/info/attributes",
        );
        matcher
    }

    fn attributes_for_path(
        &self,
        path: &[u8],
        requested: &[Vec<u8>],
        all: bool,
    ) -> Vec<AttributeCheck> {
        let mut states = BTreeMap::<Vec<u8>, Option<AttributeState>>::new();
        for pattern in &self.patterns {
            if !pattern.matches(path) {
                continue;
            }
            for assignment in &pattern.assignments {
                states.insert(assignment.attribute.clone(), assignment.state.clone());
            }
        }
        if all {
            let mut checks = states
                .into_iter()
                .filter_map(|(attribute, state)| {
                    state.map(|state| AttributeCheck {
                        attribute,
                        state: Some(state),
                    })
                })
                .collect::<Vec<_>>();
            checks.sort_by(|left, right| {
                attribute_all_rank(&left.attribute, &self.attribute_order)
                    .cmp(&attribute_all_rank(&right.attribute, &self.attribute_order))
                    .then_with(|| left.attribute.cmp(&right.attribute))
            });
            return checks;
        }
        requested
            .iter()
            .map(|attribute| AttributeCheck {
                attribute: attribute.clone(),
                state: states.get(attribute).cloned().flatten(),
            })
            .collect()
    }

    fn push_attribute_order(&mut self, attribute: &[u8]) {
        let next = self.attribute_order.len();
        self.attribute_order
            .entry(attribute.to_vec())
            .or_insert(next);
    }

    fn read_configured_attributes(&mut self, root: &Path) -> bool {
        let Ok(config) = GitConfig::read(root.join(".git").join("config")) else {
            return false;
        };
        let Some(value) = config.get("core", None, "attributesFile") else {
            return false;
        };
        let path = expand_core_excludes_file(root, value);
        read_attribute_patterns(path, self, &[], value.as_bytes());
        true
    }

    fn read_default_global_attributes(&mut self) {
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
            && !config_home.is_empty()
        {
            let path = PathBuf::from(config_home).join("git").join("attributes");
            let source = path.to_string_lossy().into_owned();
            read_attribute_patterns(path, self, &[], source.as_bytes());
            return;
        }
        if let Some(home) = std::env::var_os("HOME") {
            let path = PathBuf::from(home)
                .join(".config")
                .join("git")
                .join("attributes");
            let source = path.to_string_lossy().into_owned();
            read_attribute_patterns(path, self, &[], source.as_bytes());
        }
    }
}

/// Fold `dir`'s `.gitattributes` (if any) into `matcher`, scoped to `dir`'s path
/// within `root`. Used both by the eager full-tree pass and by the status/diff
/// worktree walk as it descends, so the tree is read for attributes exactly once.
/// Fold `dir`'s `.gitignore` (if any) into `matcher`, scoped to `dir`'s path
/// within `root`. Used by the status worktree walk as it descends so ignore
/// patterns are collected in the same traversal as worktree entries.
fn read_dir_ignore_patterns(root: &Path, dir: &Path, matcher: &mut IgnoreMatcher) -> Result<()> {
    matcher.fold_dir_ignore_patterns(root, dir)
}

fn read_dir_attribute_patterns(
    root: &Path,
    dir: &Path,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let relative = dir.strip_prefix(root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", dir.display()))
    })?;
    let base = git_path_bytes(relative)?;
    let mut source = base.clone();
    if !source.is_empty() {
        source.push(b'/');
    }
    source.extend_from_slice(b".gitattributes");
    read_attribute_patterns(dir.join(".gitattributes"), matcher, &base, &source);
    Ok(())
}

fn collect_attribute_patterns(
    root: &Path,
    dir: &Path,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    read_dir_attribute_patterns(root, dir, matcher)?;

    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        if entry.metadata()?.is_dir() {
            collect_attribute_patterns(root, &path, matcher)?;
        }
    }
    Ok(())
}

fn read_attribute_patterns(
    path: impl AsRef<Path>,
    matcher: &mut AttributeMatcher,
    base: &[u8],
    _source: &[u8],
) {
    let Ok(contents) = fs::read(path) else {
        return;
    };
    read_attribute_patterns_from_bytes(&contents, matcher, base);
}

fn read_attribute_patterns_from_bytes(
    contents: &[u8],
    matcher: &mut AttributeMatcher,
    base: &[u8],
) {
    for raw in contents.split(|byte| *byte == b'\n') {
        push_attribute_pattern(matcher, raw, base);
    }
}

fn collect_attribute_patterns_from_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    base: Vec<u8>,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let mut entries = Tree::parse(format, &object.body)?.entries;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in &entries {
        if entry.name == b".gitattributes" && tree_entry_object_type(entry.mode) == ObjectType::Blob
        {
            let object = db.read_object(&entry.oid)?;
            if object.object_type == ObjectType::Blob {
                read_attribute_patterns_from_bytes(&object.body, matcher, &base);
            }
        }
    }
    for entry in entries {
        if tree_entry_object_type(entry.mode) != ObjectType::Tree {
            continue;
        }
        let mut child_base = base.clone();
        if !child_base.is_empty() {
            child_base.push(b'/');
        }
        child_base.extend_from_slice(entry.name.as_bytes());
        collect_attribute_patterns_from_tree(db, format, &entry.oid, child_base, matcher)?;
    }
    Ok(())
}

fn collect_attribute_patterns_from_index(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(());
    }
    let mut entries = Index::parse(&fs::read(index_path)?, format)?.entries;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in entries {
        let is_attributes_file =
            entry.path == b".gitattributes" || entry.path.as_bytes().ends_with(b"/.gitattributes");
        if index_entry_stage(&entry) != 0
            || tree_entry_object_type(entry.mode) != ObjectType::Blob
            || !is_attributes_file
        {
            continue;
        }
        let base = match entry.path.as_bytes().strip_suffix(b".gitattributes") {
            Some(b"") => Vec::new(),
            Some(parent) => parent.strip_suffix(b"/").unwrap_or(parent).to_vec(),
            None => continue,
        };
        let object = db.read_object(&entry.oid)?;
        if object.object_type == ObjectType::Blob {
            read_attribute_patterns_from_bytes(&object.body, matcher, &base);
        }
    }
    Ok(())
}

fn push_attribute_pattern(matcher: &mut AttributeMatcher, raw: &[u8], base: &[u8]) {
    let line = raw.strip_suffix(b"\r").unwrap_or(raw);
    let line = trim_ascii_whitespace(line);
    if line.is_empty() || line.starts_with(b"#") {
        return;
    }
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let Some(raw_pattern) = fields.next() else {
        return;
    };
    if let Some(macro_name) = raw_pattern.strip_prefix(b"[attr]") {
        if macro_name.is_empty() {
            return;
        }
        let mut assignments = vec![AttributeAssignment {
            attribute: macro_name.to_vec(),
            state: Some(AttributeState::Set),
        }];
        for field in fields {
            push_attribute_assignments(&mut assignments, field, &matcher.macros);
        }
        for assignment in &assignments {
            matcher.push_attribute_order(&assignment.attribute);
        }
        matcher.macros.insert(macro_name.to_vec(), assignments);
        return;
    }
    let mut assignments = Vec::new();
    for field in fields {
        push_attribute_assignments(&mut assignments, field, &matcher.macros);
    }
    if assignments.is_empty() {
        return;
    }
    for assignment in &assignments {
        matcher.push_attribute_order(&assignment.attribute);
    }
    let (anchored, pattern) = if let Some(pattern) = raw_pattern.strip_prefix(b"/") {
        (true, pattern)
    } else {
        (false, raw_pattern)
    };
    if pattern.is_empty() {
        return;
    }
    matcher.patterns.push(AttributePattern {
        base: base.to_vec(),
        pattern: pattern.to_vec(),
        anchored,
        has_slash: pattern.contains(&b'/'),
        assignments,
    });
}

fn push_attribute_assignments(
    assignments: &mut Vec<AttributeAssignment>,
    field: &[u8],
    macros: &BTreeMap<Vec<u8>, Vec<AttributeAssignment>>,
) {
    if let Some(macro_assignments) = macros.get(field) {
        assignments.extend(macro_assignments.iter().cloned());
        return;
    }
    if field == b"binary" {
        assignments.push(AttributeAssignment {
            attribute: b"binary".to_vec(),
            state: Some(AttributeState::Set),
        });
        assignments.push(AttributeAssignment {
            attribute: b"diff".to_vec(),
            state: Some(AttributeState::Unset),
        });
        assignments.push(AttributeAssignment {
            attribute: b"merge".to_vec(),
            state: Some(AttributeState::Unset),
        });
        assignments.push(AttributeAssignment {
            attribute: b"text".to_vec(),
            state: Some(AttributeState::Unset),
        });
        return;
    }
    if let Some(attribute) = field.strip_prefix(b"-") {
        if !attribute.is_empty() {
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: Some(AttributeState::Unset),
            });
        }
        return;
    }
    if let Some(attribute) = field.strip_prefix(b"!") {
        if !attribute.is_empty() {
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: None,
            });
        }
        return;
    }
    if let Some(equal) = field.iter().position(|byte| *byte == b'=') {
        let attribute = &field[..equal];
        let value = &field[equal + 1..];
        if !attribute.is_empty() {
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: Some(AttributeState::Value(value.to_vec())),
            });
        }
        return;
    }
    assignments.push(AttributeAssignment {
        attribute: field.to_vec(),
        state: Some(AttributeState::Set),
    });
}

fn attribute_all_rank(
    attribute: &[u8],
    order: &BTreeMap<Vec<u8>, usize>,
) -> (usize, usize, Vec<u8>) {
    let rank = match attribute {
        b"binary" => 0,
        b"diff" => 1,
        b"merge" => 2,
        b"text" => 3,
        b"eol" => 5,
        _ => 4,
    };
    let order = order.get(attribute).copied().unwrap_or(usize::MAX);
    (rank, order, attribute.to_vec())
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

impl AttributePattern {
    fn matches(&self, path: &[u8]) -> bool {
        let path = if self.base.is_empty() {
            path
        } else {
            let Some(rest) = path
                .strip_prefix(self.base.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
            else {
                return false;
            };
            rest
        };
        if self.anchored || self.has_slash {
            return wildcard_path_matches(&self.pattern, path);
        }
        path.rsplit(|byte| *byte == b'/')
            .next()
            .is_some_and(|basename| wildcard_path_matches(&self.pattern, basename))
    }
}

// ---------------------------------------------------------------------------
// Content filtering on the blob <-> worktree boundary
//
// Git runs two kinds of conversion when content crosses between the worktree
// and the object database:
//
//   * the line-ending / `core.autocrlf` conversion (driven by the `text`,
//     `eol` attributes and the `core.autocrlf` / `core.eol` config), and
//   * the long-running `filter.<name>.clean` / `.smudge` driver filters
//     (selected by the `filter=<name>` attribute and configured commands).
//
// "clean" runs on the way *into* the object store (worktree -> blob), e.g. on
// `git add` / `git hash-object -w`. "smudge" runs on the way *out* (blob ->
// worktree), e.g. on checkout / restore. The driver filter, when present,
// wraps the EOL conversion: on clean git first runs the configured `clean`
// command and then applies CRLF->LF normalization; on smudge git first applies
// LF->CRLF and then runs the `smudge` command.
// ---------------------------------------------------------------------------

/// The line-ending conversion that applies to a path, derived from its
/// attributes and the repository config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EolConversion {
    /// No conversion: binary content, or text with `core.autocrlf=false` and no
    /// `eol`/`text=auto` request to add carriage returns.
    None,
    /// Normalize to LF on clean; no carriage returns on smudge (`eol=lf`, or
    /// `core.autocrlf=input`).
    Lf,
    /// Normalize to LF on clean; emit CRLF on smudge (`eol=crlf`, or
    /// `core.autocrlf=true`).
    Crlf,
}

/// How git should decide whether a path is text for the purpose of EOL
/// conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextDecision {
    /// `-text` / `binary`: never convert.
    Binary,
    /// `text` is set explicitly: always treat as text.
    Text,
    /// `text=auto` (or implied by `core.autocrlf`): treat as text unless the
    /// content looks binary.
    Auto,
    /// No opinion from attributes or config: leave content untouched.
    Unspecified,
}

/// The fully resolved set of conversions that apply to a single path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContentFilterPlan {
    text: TextDecision,
    /// The conversion to apply when `text` resolves to "this is text".
    eol: EolConversion,
    /// `filter.<name>` driver, if assigned via attributes and configured.
    driver: Option<FilterDriver>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilterDriver {
    name: Vec<u8>,
    clean: Option<String>,
    smudge: Option<String>,
    required: bool,
}

impl ContentFilterPlan {
    /// Build the plan for `path` from the parsed attributes and repo config.
    fn resolve(config: &GitConfig, checks: &[AttributeCheck]) -> Self {
        let text_attr = checks.iter().find(|check| check.attribute == b"text");
        let eol_attr = checks.iter().find(|check| check.attribute == b"eol");
        let filter_attr = checks.iter().find(|check| check.attribute == b"filter");

        // Resolve the eol attribute first; `eol=crlf|lf` also forces text.
        let eol_value = eol_attr.and_then(|check| match &check.state {
            Some(AttributeState::Value(value)) => Some(value.clone()),
            _ => None,
        });

        let mut text = match text_attr.map(|check| &check.state) {
            Some(Some(AttributeState::Set)) => TextDecision::Text,
            Some(Some(AttributeState::Unset)) => TextDecision::Binary,
            Some(Some(AttributeState::Value(value))) if value == b"auto" => TextDecision::Auto,
            // `text=<other>` is treated by git as a set text attribute.
            Some(Some(AttributeState::Value(_))) => TextDecision::Text,
            // `!text` (unspecified) or no text attribute: fall through.
            _ => TextDecision::Unspecified,
        };

        // A concrete `eol` attribute implies the path is text even when `text`
        // was left unspecified (git: `eol` without `text` is treated as
        // `text=auto`-ish; upstream forces conversion). We honour eol only when
        // text is not explicitly binary.
        let eol = match (&text, eol_value.as_deref()) {
            (TextDecision::Binary, _) => EolConversion::None,
            (_, Some(b"crlf")) => {
                if text == TextDecision::Unspecified {
                    text = TextDecision::Text;
                }
                EolConversion::Crlf
            }
            (_, Some(b"lf")) => {
                if text == TextDecision::Unspecified {
                    text = TextDecision::Text;
                }
                EolConversion::Lf
            }
            // No eol attribute: derive direction from config.
            _ => eol_from_config(config),
        };

        // When the path is text but neither `eol` nor `core.autocrlf`/`core.eol`
        // asked for carriage returns, we still normalize to LF on clean. That is
        // modelled by `EolConversion::Lf` (clean strips CR, smudge adds none).
        let eol = match (&text, eol) {
            (TextDecision::Text | TextDecision::Auto, EolConversion::None) => EolConversion::Lf,
            (_, eol) => eol,
        };

        // If config does not enable autocrlf and there is no eol/text opinion,
        // there is genuinely nothing to do.
        let text = match (text, eol_attr.is_some()) {
            (TextDecision::Unspecified, _) => {
                // Without any text/eol attribute, only `core.autocrlf` can make a
                // path eligible, and then it behaves like `text=auto`.
                if autocrlf_enabled(config) {
                    TextDecision::Auto
                } else {
                    TextDecision::Unspecified
                }
            }
            (text, _) => text,
        };

        let driver = resolve_filter_driver(config, filter_attr);

        ContentFilterPlan { text, eol, driver }
    }

    /// Whether EOL conversion should run for the given content.
    fn convert_eol(&self, content: &[u8]) -> bool {
        match self.text {
            TextDecision::Binary | TextDecision::Unspecified => false,
            TextDecision::Text => self.eol != EolConversion::None,
            // `text=auto`: only when the blob does not look binary.
            TextDecision::Auto => self.eol != EolConversion::None && !looks_binary(content),
        }
    }
}

/// Derive the smudge-direction line ending from `core.autocrlf` / `core.eol`.
fn eol_from_config(config: &GitConfig) -> EolConversion {
    if let Some(value) = config.get("core", None, "autocrlf") {
        match value.to_ascii_lowercase().as_str() {
            "input" => return EolConversion::Lf,
            "true" | "yes" | "on" | "1" => return EolConversion::Crlf,
            _ => {}
        }
    }
    if config.get_bool("core", None, "autocrlf") == Some(true) {
        return EolConversion::Crlf;
    }
    match config
        .get("core", None, "eol")
        .map(|v| v.to_ascii_lowercase())
    {
        Some(ref v) if v == "crlf" => EolConversion::Crlf,
        Some(ref v) if v == "lf" => EolConversion::Lf,
        _ => EolConversion::None,
    }
}

/// Whether `core.autocrlf` is set to anything that enables conversion
/// (`true` or `input`).
fn autocrlf_enabled(config: &GitConfig) -> bool {
    if let Some(value) = config.get("core", None, "autocrlf")
        && value.eq_ignore_ascii_case("input")
    {
        return true;
    }
    config.get_bool("core", None, "autocrlf") == Some(true)
}

/// Resolve the `filter=<name>` attribute against `filter.<name>.*` config.
fn resolve_filter_driver(
    config: &GitConfig,
    filter_attr: Option<&AttributeCheck>,
) -> Option<FilterDriver> {
    let name = match filter_attr.map(|check| &check.state) {
        Some(Some(AttributeState::Value(value))) => value.clone(),
        // `filter` set/unset without a value selects no driver.
        _ => return None,
    };
    let subsection = String::from_utf8_lossy(&name).into_owned();
    let clean = config
        .get("filter", Some(&subsection), "clean")
        .filter(|cmd| !cmd.is_empty())
        .map(str::to_owned);
    let smudge = config
        .get("filter", Some(&subsection), "smudge")
        .filter(|cmd| !cmd.is_empty())
        .map(str::to_owned);
    let required = config
        .get_bool("filter", Some(&subsection), "required")
        .unwrap_or(false);
    // A filter with neither command and not required is a no-op.
    if clean.is_none() && smudge.is_none() && !required {
        return None;
    }
    Some(FilterDriver {
        name,
        clean,
        smudge,
        required,
    })
}

/// Heuristic mirroring git's `buffer_is_binary`: content is treated as binary
/// when a NUL byte appears within the first 8000 bytes.
fn looks_binary(content: &[u8]) -> bool {
    const FIRST_FEW_BYTES: usize = 8000;
    let window = &content[..content.len().min(FIRST_FEW_BYTES)];
    window.contains(&0)
}

/// Strip carriage returns that immediately precede a line feed (CRLF -> LF).
/// A lone CR (old-Mac line ending) is left untouched, matching git, which only
/// collapses CRLF pairs.
fn convert_crlf_to_lf(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len());
    let mut index = 0;
    while index < content.len() {
        let byte = content[index];
        if byte == b'\r' && content.get(index + 1) == Some(&b'\n') {
            // Drop the CR; the LF is emitted on the next iteration.
            index += 1;
            continue;
        }
        out.push(byte);
        index += 1;
    }
    out
}

/// Convert lone LF bytes to CRLF (LF -> CRLF). An LF already preceded by a CR
/// is left as-is so content is not double-converted, matching git.
fn convert_lf_to_crlf(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + content.len() / 16);
    let mut prev = 0u8;
    for &byte in content {
        if byte == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(byte);
        prev = byte;
    }
    out
}

/// Run a configured `clean`/`smudge` command as a subprocess, feeding `content`
/// on stdin and returning its stdout. Errors carry enough context for the
/// caller to decide whether the failure is fatal (required filter) or should be
/// silently ignored (optional filter passthrough).
fn run_filter_command(command: &str, path: &[u8], content: &[u8]) -> Result<Vec<u8>> {
    // Git expands `%f` in the filter command to the path of the file being
    // filtered (quoted). We perform the same substitution.
    let display_path = String::from_utf8_lossy(path);
    let expanded = command.replace("%f", &shell_quote(&display_path));
    // Run through the platform shell so pipelines / arguments in the configured
    // command behave the same way git's `run_command`-with-shell does.
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("/bin/sh", "-c")
    };
    let mut child = Command::new(shell)
        .arg(flag)
        .arg(&expanded)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(format!("failed to spawn filter `{command}`: {err}")))?;
    // Write the content to the child's stdin on a separate thread so we never
    // deadlock against a filter that streams output before consuming all input.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| GitError::Command(format!("filter `{command}` stdin unavailable")))?;
    let payload = content.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
        // Dropping `stdin` here closes the pipe so the child sees EOF.
    });
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(format!("filter `{command}` failed: {err}")))?;
    // Join the writer; its own errors (e.g. broken pipe) are non-fatal because
    // the child's exit status is the authoritative signal.
    let _ = writer.join();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::Command(format!(
            "filter `{command}` exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(output.stdout)
}

/// Minimal POSIX single-quote escaping for substituting `%f` into a shell
/// command (used only for the path passed to driver filters).
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Apply the *clean* conversion to `content` for `path` (worktree -> blob):
/// first the configured `filter.<name>.clean` driver (if any), then CRLF->LF
/// normalization when EOL conversion applies.
///
/// `config` is the repository config (`GitConfig`) and `path` is the
/// repository-relative path of the file (forward-slash separated, e.g.
/// `src/main.rs`). When no filter or EOL conversion applies the input is
/// returned unchanged.
///
/// A *required* driver (`filter.<name>.required=true`) whose `clean` command is
/// missing or fails produces a [`GitError::Command`]; a non-required driver
/// failure (or absence of a `clean` command) passes the content through
/// unfiltered, matching git.
pub fn apply_clean_filter(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    config: &GitConfig,
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    // On clean the worktree file exists, so the live `.gitattributes` chain is
    // authoritative. `git_dir` is accepted for symmetry with the smudge entry
    // point (which falls back to the index) and for future use.
    let _ = git_dir.as_ref();
    let checks = filter_attribute_checks(worktree_root.as_ref(), path)?;
    apply_clean_filter_with_attributes(config, &checks, path, content)
}

/// Like [`apply_clean_filter`] but takes already-resolved attribute checks,
/// letting callers that have computed attributes once reuse them.
pub fn apply_clean_filter_with_attributes(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    Ok(apply_clean_filter_with_attributes_cow(config, attributes, path, content)?.into_owned())
}

/// Borrow-first variant of [`apply_clean_filter_with_attributes`].
///
/// When no filter or EOL conversion changes the content, the returned value
/// borrows `content`; callers that can consume a [`Cow`] avoid allocating for
/// the common pass-through case.
pub fn apply_clean_filter_with_attributes_cow<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
) -> Result<Cow<'a, [u8]>> {
    let plan = ContentFilterPlan::resolve(config, attributes);
    let mut data = Cow::Borrowed(content);
    if let Some(driver) = &plan.driver {
        data = run_driver(driver, driver.clean.as_deref(), path, data)?;
    }
    if plan.convert_eol(&data) {
        data = Cow::Owned(convert_crlf_to_lf(&data));
    }
    Ok(data)
}

/// Apply the *smudge* conversion to `content` for `path` (blob -> worktree):
/// first LF->CRLF when EOL conversion applies, then the configured
/// `filter.<name>.smudge` driver (if any).
///
/// Semantics mirror [`apply_clean_filter`]: a required driver with a missing or
/// failing `smudge` command errors, while a non-required one passes the content
/// through.
pub fn apply_smudge_filter(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    config: &GitConfig,
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    // On smudge (checkout) the worktree file may not exist yet, so resolve the
    // attributes from the `.gitattributes` recorded in the index.
    let checks =
        smudge_attribute_checks_from_index(worktree_root.as_ref(), git_dir.as_ref(), format, path)?;
    apply_smudge_filter_with_attributes(config, &checks, path, content)
}

/// Like [`apply_smudge_filter`] but takes already-resolved attribute checks.
pub fn apply_smudge_filter_with_attributes(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    Ok(apply_smudge_filter_with_attributes_cow(config, attributes, path, content)?.into_owned())
}

/// Borrow-first variant of [`apply_smudge_filter_with_attributes`].
///
/// When no filter or EOL conversion changes the content, the returned value
/// borrows `content`; callers that can consume a [`Cow`] avoid allocating for
/// the common pass-through case.
pub fn apply_smudge_filter_with_attributes_cow<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
) -> Result<Cow<'a, [u8]>> {
    let plan = ContentFilterPlan::resolve(config, attributes);
    let mut data = Cow::Borrowed(content);
    if plan.eol == EolConversion::Crlf && plan.convert_eol(&data) {
        data = Cow::Owned(convert_lf_to_crlf(&data));
    }
    if let Some(driver) = &plan.driver {
        data = run_driver(driver, driver.smudge.as_deref(), path, data)?;
    }
    Ok(data)
}

/// Execute one direction of a driver filter, honouring the `required` flag.
fn run_driver<'a>(
    driver: &FilterDriver,
    command: Option<&str>,
    path: &[u8],
    content: Cow<'a, [u8]>,
) -> Result<Cow<'a, [u8]>> {
    let Some(command) = command else {
        // No command in this direction. Required filters must error; optional
        // ones pass content through unchanged.
        if driver.required {
            return Err(GitError::Command(format!(
                "required filter `{}` has no configured command for this direction",
                String::from_utf8_lossy(&driver.name)
            )));
        }
        return Ok(content);
    };
    match run_filter_command(command, path, &content) {
        Ok(output) => Ok(Cow::Owned(output)),
        Err(err) => {
            if driver.required {
                Err(err)
            } else {
                // Non-required filter failure: fall back to the unfiltered
                // content, matching git's behaviour.
                Ok(content)
            }
        }
    }
}

/// Compute the attributes relevant to content filtering (`text`, `eol`,
/// `filter`) for `path` from the worktree `.gitattributes` chain.
fn filter_attribute_checks(worktree_root: &Path, path: &[u8]) -> Result<Vec<AttributeCheck>> {
    let requested = filter_attribute_names();
    standard_attributes_for_path(worktree_root, path, &requested, false)
}

/// Compute filtering attributes for a checkout (blob -> worktree), reading
/// `.gitattributes` from the index so the rules in the tree being checked out
/// apply even before the worktree files exist.
fn smudge_attribute_checks_from_index(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    path: &[u8],
) -> Result<Vec<AttributeCheck>> {
    let requested = filter_attribute_names();
    standard_attributes_for_path_from_index(worktree_root, git_dir, format, path, &requested, false)
}

fn filter_attribute_names() -> Vec<Vec<u8>> {
    vec![b"text".to_vec(), b"eol".to_vec(), b"filter".to_vec()]
}

pub fn deleted_index_entries(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<IndexEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    let mut deleted = Vec::new();
    for entry in index.entries {
        if !worktree_path(worktree_root, entry.path.as_bytes())?.exists() {
            deleted.push(entry);
        }
    }
    Ok(deleted)
}

pub fn modified_index_entries(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<IndexEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    // Reuse the same racy-git stat shortcut here: build the cache from the index
    // we just parsed (no second parse) so the worktree walk can skip re-hashing
    // unchanged files. A cached oid is only trusted on a non-racy stat match, so
    // genuinely modified files still fall through to a hash and are reported.
    let stat_cache = IndexStatCache::from_index(&index, &index_path);
    let worktree = worktree_entries_with_stat_cache(
        worktree_root,
        git_dir,
        format,
        Some(&stat_cache),
        None,
        None,
    )?;
    let mut modified = Vec::new();
    for entry in index.entries {
        let Some(worktree_entry) = worktree.get(entry.path.as_bytes()) else {
            modified.push(entry);
            continue;
        };
        if worktree_entry.mode != entry.mode || worktree_entry.oid != entry.oid {
            modified.push(entry);
        }
    }
    Ok(modified)
}

pub fn checkout_branch(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    branch: &str,
    committer: Vec<u8>,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let branch_ref = branch_ref_name(branch)?;
    let refs = FileRefStore::new(git_dir, format);
    let target = match sley_refs::resolve_ref_peeled(&refs, &branch_ref)? {
        Some(oid) => oid,
        None => {
            checkout_switch_head_symbolic(&refs, branch_ref, committer, branch, None, None)?;
            return Ok(CheckoutResult {
                branch: branch.into(),
                oid: ObjectId::null(format),
                files: 0,
            });
        }
    };
    let current_head = resolve_head_commit_oid(git_dir, format)?;
    let files = if current_head == Some(target) {
        0
    } else {
        checkout_commit_to_index_and_worktree(worktree_root, git_dir, format, &target)?
    };
    checkout_switch_head_symbolic(
        &refs,
        branch_ref,
        committer,
        branch,
        Some(target),
        Some(target),
    )?;
    Ok(CheckoutResult {
        branch: branch.into(),
        oid: target,
        files,
    })
}

pub fn checkout_detached(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    target: &ObjectId,
    committer: Vec<u8>,
    message: Vec<u8>,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let files = checkout_commit_to_index_and_worktree(worktree_root, git_dir, format, target)?;
    let refs = FileRefStore::new(git_dir, format);
    let zero = ObjectId::null(format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid: zero,
            new_oid: *target,
            committer,
            message,
        }),
    });
    tx.commit()?;
    Ok(CheckoutResult {
        branch: target.to_string(),
        oid: *target,
        files,
    })
}

/// Like [`checkout_branch`], but runs the smudge-side content filters
/// (`core.autocrlf`/`text`/`eol` EOL conversion and `filter.<name>.smudge`
/// drivers) on each blob as it is written to the worktree. `config` is the
/// repository config used to resolve the filters.
pub fn checkout_branch_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    branch: &str,
    committer: Vec<u8>,
    config: &GitConfig,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let branch_ref = branch_ref_name(branch)?;
    let refs = FileRefStore::new(git_dir, format);
    let target = match sley_refs::resolve_ref_peeled(&refs, &branch_ref)? {
        Some(oid) => oid,
        None => {
            checkout_switch_head_symbolic(&refs, branch_ref, committer, branch, None, None)?;
            return Ok(CheckoutResult {
                branch: branch.into(),
                oid: ObjectId::null(format),
                files: 0,
            });
        }
    };
    let current_head = resolve_head_commit_oid(git_dir, format)?;
    let files = if current_head == Some(target) {
        0
    } else {
        checkout_commit_to_index_and_worktree_filtered(
            worktree_root,
            git_dir,
            format,
            &target,
            Some(config),
        )?
    };
    checkout_switch_head_symbolic(
        &refs,
        branch_ref,
        committer,
        branch,
        Some(target),
        Some(target),
    )?;
    Ok(CheckoutResult {
        branch: branch.into(),
        oid: target,
        files,
    })
}

/// Like [`checkout_detached`], but runs the smudge-side content filters (see
/// [`checkout_branch_filtered`]).
pub fn checkout_detached_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    target: &ObjectId,
    committer: Vec<u8>,
    message: Vec<u8>,
    config: &GitConfig,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let files = checkout_commit_to_index_and_worktree_filtered(
        worktree_root,
        git_dir,
        format,
        target,
        Some(config),
    )?;
    let refs = FileRefStore::new(git_dir, format);
    let zero = ObjectId::null(format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid: zero,
            new_oid: *target,
            committer,
            message,
        }),
    });
    tx.commit()?;
    Ok(CheckoutResult {
        branch: target.to_string(),
        oid: *target,
        files,
    })
}

fn checkout_commit_to_index_and_worktree(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
) -> Result<usize> {
    checkout_commit_to_index_and_worktree_filtered(worktree_root, git_dir, format, target, None)
}

/// Like [`checkout_commit_to_index_and_worktree`] but optionally runs the
/// smudge-side content filters (see [`apply_smudge_filter`]) on each blob before
/// it is written to the worktree. Attribute lookups use the `.gitattributes`
/// recorded in the *target tree* so the rules of the checked-out commit apply.
fn checkout_commit_to_index_and_worktree_filtered(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    smudge_config: Option<&GitConfig>,
) -> Result<usize> {
    let status = short_status(worktree_root, git_dir, format)?;
    if status
        .iter()
        .any(|entry| !status_entry_is_untracked_or_ignored(entry))
    {
        return Err(GitError::Transaction(
            "checkout requires a clean working tree".into(),
        ));
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, target)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;

    let attributes = smudge_config
        .map(|_| build_tree_attribute_matcher(worktree_root, &db, format, &commit.tree))
        .transpose()?;

    for path in read_index_entries(git_dir, format)?.keys() {
        if !target_entries.contains_key(path) {
            remove_worktree_file(worktree_root, path)?;
        }
    }

    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        let object = db.read_object(&entry.oid)?;
        if object.object_type != ObjectType::Blob {
            return Err(GitError::InvalidObject(format!(
                "expected blob {}, found {}",
                entry.oid,
                object.object_type.as_str()
            )));
        }
        let body: Cow<'_, [u8]> = match (smudge_config, &attributes) {
            (Some(config), Some(matcher)) => {
                let checks = matcher.attributes_for_path(path, &filter_attribute_names(), false);
                apply_smudge_filter_with_attributes_cow(config, &checks, path, &object.body)?
            }
            _ => Cow::Borrowed(&object.body),
        };
        let file_path = worktree_path(worktree_root, path)?;
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file_path, &body)?;
        let metadata = fs::metadata(&file_path)?;
        let mut index_entry = index_entry_from_metadata(path.clone(), entry.oid, &metadata);
        index_entry.mode = entry.mode;
        index_entries.push(index_entry);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(target_entries.len())
}

/// Build an [`AttributeMatcher`] from the `.gitattributes` files contained in a
/// tree, plus the repo-level (`core.attributesFile`, `.git/info/attributes`)
/// sources, mirroring [`standard_attributes_for_path_from_tree`].
fn build_tree_attribute_matcher(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<AttributeMatcher> {
    let mut matcher = AttributeMatcher::default();
    if !matcher.read_configured_attributes(worktree_root) {
        matcher.read_default_global_attributes();
    }
    collect_attribute_patterns_from_tree(db, format, tree_oid, Vec::new(), &mut matcher)?;
    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
    );
    Ok(matcher)
}

/// Sparse- and skip-worktree-aware variant of
/// [`checkout_commit_to_index_and_worktree`].
///
/// When `sparse` is `None` this behaves like the plain checkout except that it
/// preserves any pre-existing skip-worktree bits (so an already-sparse worktree
/// is not silently re-expanded). When `sparse` is `Some`, every target path is
/// additionally classified against the patterns: in-cone paths are written and
/// have their skip-worktree bit cleared, while out-of-cone paths are left out
/// of the worktree, get their skip-worktree bit set, and have any stale file
/// removed.
fn checkout_commit_to_index_and_worktree_sparse(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    sparse: Option<(&SparseCheckout, SparseCheckoutMode)>,
) -> Result<usize> {
    let previously_skipped = skip_worktree_paths(git_dir, format)?;
    // Honor skip-worktree: a path whose worktree file is intentionally absent
    // must not be treated as a dirty (deleted) change blocking the checkout.
    let status = short_status(worktree_root, git_dir, format)?;
    if status
        .iter()
        .any(|entry| !previously_skipped.contains(entry.path.as_slice()))
    {
        return Err(GitError::Transaction(
            "checkout requires a clean working tree".into(),
        ));
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, target)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;

    let matcher = sparse.map(|(spec, mode)| SparseMatcher::new(spec, mode));

    for path in read_index_entries(git_dir, format)?.keys() {
        if target_entries.contains_key(path) {
            continue;
        }
        // Do not disturb the worktree state of an intentionally skipped path.
        if previously_skipped.contains(path) {
            continue;
        }
        remove_worktree_file(worktree_root, path)?;
    }

    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        let in_cone = matcher.as_ref().is_none_or(|matcher| {
            // A path already marked skip-worktree stays out unless it now
            // matches the sparse cone, mirroring upstream "honor skip-worktree".
            matcher.includes_file(path)
        });
        let index_entry = if in_cone {
            let object = db.read_object(&entry.oid)?;
            if object.object_type != ObjectType::Blob {
                return Err(GitError::InvalidObject(format!(
                    "expected blob {}, found {}",
                    entry.oid,
                    object.object_type.as_str()
                )));
            }
            let file_path = worktree_path(worktree_root, path)?;
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&file_path, &object.body)?;
            let metadata = fs::metadata(&file_path)?;
            let mut index_entry = index_entry_from_metadata(path.clone(), entry.oid, &metadata);
            index_entry.mode = entry.mode;
            // `index_entry_from_metadata` leaves flags_extended at 0, so the
            // skip-worktree bit is already clear for in-cone paths.
            index_entry
        } else {
            // Out of cone: ensure no stale worktree file remains and synthesize
            // an index entry straight from the tree (no worktree metadata),
            // then mark it skip-worktree.
            remove_worktree_file(worktree_root, path)?;
            let mut index_entry = restored_head_index_entry(worktree_root, &db, path, entry)?;
            set_skip_worktree(&mut index_entry);
            index_entry
        };
        index_entries.push(index_entry);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut index = Index {
        version: 2,
        entries: index_entries,
        extensions: Vec::new(),
        checksum: None,
    };
    normalize_index_version_for_extended_flags(&mut index);
    fs::write(repository_index_path(git_dir), index.write(format)?)?;
    Ok(target_entries.len())
}

fn skip_worktree_paths(git_dir: &Path, format: ObjectFormat) -> Result<BTreeSet<Vec<u8>>> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeSet::new());
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    Ok(index
        .entries
        .into_iter()
        .filter(index_entry_skip_worktree)
        .map(|entry| entry.path.into_bytes())
        .collect())
}

pub fn restore_worktree_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Err(GitError::Exit(1));
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut restored = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        let recursive = path == Path::new(".")
            || path.to_string_lossy().ends_with('/')
            || absolute.is_dir()
            || index_has_entry_under(&index.entries, &git_path);
        let mut matched = false;
        for entry in &index.entries {
            if entry.path.as_bytes() == git_path.as_slice()
                || (recursive && index_entry_is_under_path(entry.path.as_bytes(), &git_path))
            {
                restore_index_entry(worktree_root, &db, entry)?;
                restored.insert(entry.path.clone());
                matched = true;
            }
        }
        if !matched {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                path.display()
            );
            return Err(GitError::Exit(1));
        }
    }
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn restore_index_paths_from_head(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &head_entries,
        paths,
    )
}

pub fn restore_index_paths_from_tree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let source_entries = tree_entries(&db, format, tree_oid)?;
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
        paths,
    )
}

fn restore_index_paths_from_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let mut index_entries = index
        .entries
        .into_iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut restored = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        let recursive = path == Path::new(".")
            || path.to_string_lossy().ends_with('/')
            || absolute.is_dir()
            || index_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path))
            || source_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path));
        let mut matched_paths = BTreeSet::new();
        for path in index_entries.keys().chain(source_entries.keys()) {
            if path.as_slice() == git_path.as_slice()
                || (recursive && index_entry_is_under_path(path, &git_path))
            {
                matched_paths.insert(path.clone());
            }
        }
        if matched_paths.is_empty() {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                path.display()
            );
            return Err(GitError::Exit(1));
        }
        for path in matched_paths {
            if let Some(entry) = source_entries.get(&path) {
                index_entries.insert(
                    path.clone(),
                    restored_head_index_entry(worktree_root, db, &path, entry)?,
                );
            } else {
                index_entries.remove(&path);
            }
            restored.insert(path);
        }
    }
    let mut entries = index_entries.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn restore_index_and_worktree_paths_from_head(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    restore_index_and_worktree_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &head_entries,
        paths,
    )
}

pub fn restore_index_and_worktree_paths_from_tree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let source_entries = tree_entries(&db, format, tree_oid)?;
    restore_index_and_worktree_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
        paths,
    )
}

fn restore_index_and_worktree_paths_from_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let mut index_entries = index
        .entries
        .into_iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut restored = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        let recursive = path == Path::new(".")
            || path.to_string_lossy().ends_with('/')
            || absolute.is_dir()
            || index_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path))
            || source_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path));
        let mut matched_paths = BTreeSet::new();
        for path in index_entries.keys().chain(source_entries.keys()) {
            if path.as_slice() == git_path.as_slice()
                || (recursive && index_entry_is_under_path(path, &git_path))
            {
                matched_paths.insert(path.clone());
            }
        }
        if matched_paths.is_empty() {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                path.display()
            );
            return Err(GitError::Exit(1));
        }
        for path in matched_paths {
            if let Some(entry) = source_entries.get(&path) {
                index_entries.insert(
                    path.clone(),
                    restore_head_entry_to_worktree_and_index(worktree_root, db, &path, entry)?,
                );
            } else {
                index_entries.remove(&path);
                remove_worktree_file(worktree_root, &path)?;
            }
            restored.insert(path);
        }
    }
    let mut entries = index_entries.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn reset_index_and_worktree_to_commit(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, commit_oid)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;

    for path in read_index_entries(git_dir, format)?.keys() {
        if !target_entries.contains_key(path) {
            remove_worktree_file(worktree_root, path)?;
        }
    }

    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        let object = db.read_object(&entry.oid)?;
        if object.object_type != ObjectType::Blob {
            return Err(GitError::InvalidObject(format!(
                "expected blob {}, found {}",
                entry.oid,
                object.object_type.as_str()
            )));
        }
        let file_path = worktree_path(worktree_root, path)?;
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file_path, &object.body)?;
        let metadata = fs::metadata(&file_path)?;
        let mut index_entry = index_entry_from_metadata(path.clone(), entry.oid, &metadata);
        index_entry.mode = entry.mode;
        index_entries.push(index_entry);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: target_entries.len(),
    })
}

/// Materialize a tree object into the index and worktree.
pub fn checkout_tree_to_index_and_worktree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, tree_oid, &mut target_entries)?;

    for path in read_index_entries(git_dir, format)?.keys() {
        if !target_entries.contains_key(path) {
            remove_worktree_file(worktree_root, path)?;
        }
    }

    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        let object = db.read_object(&entry.oid)?;
        if object.object_type != ObjectType::Blob {
            return Err(GitError::InvalidObject(format!(
                "expected blob {}, found {}",
                entry.oid,
                object.object_type.as_str()
            )));
        }
        let file_path = worktree_path(worktree_root, path)?;
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file_path, &object.body)?;
        let metadata = fs::metadata(&file_path)?;
        let mut index_entry = index_entry_from_metadata(path.clone(), entry.oid, &metadata);
        index_entry.mode = entry.mode;
        index_entries.push(index_entry);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: target_entries.len(),
    })
}

pub fn reset_index_to_commit(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, commit_oid)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;
    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        index_entries.push(restored_head_index_entry(worktree_root, &db, path, entry)?);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: target_entries.len(),
    })
}

/// Build a fresh in-memory index that mirrors the tree `tree_oid`, the way
/// `git read-tree <tree>` does: every blob, symlink, and gitlink leaf (found by
/// recursing subtrees) becomes a stage-0 entry carrying the tree mode and oid,
/// with a fully zeroed stat (so nothing is treated as stat-clean) and size 0.
/// Entries are sorted by path; the index is version 2 with no extensions.
///
/// This does not touch the worktree or write anything to disk — serialize the
/// result with [`Index::write`] (and persist it) when you want to replace
/// `.git/index`.
pub fn index_from_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<Index> {
    let mut entries: Vec<IndexEntry> = Vec::new();
    if *tree_oid != ObjectId::empty_tree(format) {
        let mut tree_entries = BTreeMap::new();
        collect_tree_entries(db, format, tree_oid, &mut tree_entries)?;
        entries.reserve(tree_entries.len());
        for (path, entry) in tree_entries {
            let name_len = (path.len().min(0x0fff)) as u16;
            entries.push(IndexEntry {
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
                flags: name_len,
                flags_extended: 0,
                path: path.into(),
            });
        }
    }
    // git orders index entries by path bytes; BTreeMap already yields that, but
    // sort explicitly so the contract holds regardless of how entries arrive.
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    })
}

/// Enforces a [`SparseCheckout`] against the current index and worktree.
///
/// Every stage-0 index entry is classified with the sparse patterns (see
/// [`SparseCheckoutMode`] for the matching semantics):
///
/// * **In cone**: the skip-worktree bit is cleared and, if the worktree file is
///   missing, it is re-materialized from the entry's blob in the object
///   database. Existing worktree files are left untouched so local content is
///   preserved.
/// * **Out of cone**: the skip-worktree bit is set and any existing worktree
///   file is removed (empty parent directories are pruned).
///
/// Conflicted entries (stage != 0) are never given the skip-worktree bit and
/// are left alone, matching upstream Git. The index is rewritten in place.
pub fn apply_sparse_checkout(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    sparse: &SparseCheckout,
) -> Result<ApplySparseResult> {
    apply_sparse_checkout_with_mode(
        worktree_root,
        git_dir,
        format,
        sparse,
        SparseCheckoutMode::Auto,
    )
}

/// Like [`apply_sparse_checkout`] but lets the caller force the pattern
/// interpretation instead of auto-detecting it.
pub fn apply_sparse_checkout_with_mode(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    sparse: &SparseCheckout,
    mode: SparseCheckoutMode,
) -> Result<ApplySparseResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        return Ok(ApplySparseResult {
            materialized: Vec::new(),
            skipped: Vec::new(),
            not_up_to_date: Vec::new(),
        });
    };
    let matcher = SparseMatcher::new(sparse, mode);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut materialized = Vec::new();
    let mut skipped = Vec::new();
    let mut not_up_to_date = Vec::new();
    for entry in &mut index.entries {
        // Never touch conflicted entries.
        if index_entry_stage(entry) != 0 {
            continue;
        }
        if matcher.includes_file(entry.path.as_bytes()) {
            clear_skip_worktree(entry);
            let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
            if !file_path.exists() {
                materialize_index_entry_file(&db, &file_path, entry)?;
            }
            materialized.push(entry.path.as_bytes().to_vec());
        } else {
            // The path is out of cone, so its worktree file should be removed and
            // the entry marked skip-worktree. But git refuses to delete a file
            // that is *not up to date* with the index (e.g. one that reappeared in
            // the worktree after the path was already sparse): it leaves the file,
            // leaves the skip-worktree bit clear, and reports the path in its "not
            // up to date" warning. Mirror that to avoid silent data loss.
            let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
            match fs::symlink_metadata(&file_path) {
                Ok(metadata) if !worktree_entry_is_uptodate(entry, &metadata) => {
                    clear_skip_worktree(entry);
                    not_up_to_date.push(entry.path.as_bytes().to_vec());
                }
                _ => {
                    set_skip_worktree(entry);
                    remove_worktree_file(worktree_root, entry.path.as_bytes())?;
                    skipped.push(entry.path.as_bytes().to_vec());
                }
            }
        }
    }
    not_up_to_date.sort();
    normalize_index_version_for_extended_flags(&mut index);
    fs::write(index_path, index.write(format)?)?;
    Ok(ApplySparseResult {
        materialized,
        skipped,
        not_up_to_date,
    })
}

/// Whether the worktree file described by `metadata` is up to date with `entry`'s
/// cached index stat, using the size + mtime heuristic at the core of git's
/// `ie_match_stat`. A freshly-checked-out (clean) file matches; a file that was
/// deleted and later recreated — as happens when an out-of-cone path reappears in
/// the worktree — gets a fresh mtime and so reads as modified, which is exactly
/// the state git declines to overwrite during a sparse update.
fn worktree_entry_is_uptodate(entry: &IndexEntry, metadata: &fs::Metadata) -> bool {
    if u64::from(entry.size) != metadata.len() {
        return false;
    }
    let Some((mtime_seconds, mtime_nanoseconds)) = file_mtime_parts(metadata) else {
        // Without a usable mtime we cannot prove the file is clean; treat it as
        // not up to date so a present file is never silently discarded.
        return false;
    };
    u64::from(entry.mtime_seconds) == mtime_seconds
        && u64::from(entry.mtime_nanoseconds) == mtime_nanoseconds
}

/// The file's modification time split into whole seconds and the sub-second
/// nanosecond remainder, matching how git stores `mtime` in the index.
fn file_mtime_parts(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((duration.as_secs(), u64::from(duration.subsec_nanos())))
}

/// Checks out `target` like [`checkout_detached`], but materializes the
/// worktree through the supplied [`SparseCheckout`]: out-of-cone paths are not
/// written, get their skip-worktree bit set, and have any stale worktree file
/// removed. Existing public checkout entry points are unchanged; this is an
/// additive sparse-aware variant.
///
/// The pattern interpretation is auto-detected ([`SparseCheckoutMode::Auto`]);
/// to reconcile an existing checkout under an explicit mode use
/// [`apply_sparse_checkout_with_mode`].
pub fn checkout_detached_sparse(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    target: &ObjectId,
    committer: Vec<u8>,
    message: Vec<u8>,
    sparse: &SparseCheckout,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let files = checkout_commit_to_index_and_worktree_sparse(
        worktree_root,
        git_dir,
        format,
        target,
        Some((sparse, SparseCheckoutMode::Auto)),
    )?;
    let refs = FileRefStore::new(git_dir, format);
    let zero = ObjectId::null(format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid: zero,
            new_oid: *target,
            committer,
            message,
        }),
    });
    tx.commit()?;
    Ok(CheckoutResult {
        branch: target.to_string(),
        oid: *target,
        files,
    })
}

fn materialize_index_entry_file(
    db: &FileObjectDatabase,
    file_path: &Path,
    entry: &IndexEntry,
) -> Result<()> {
    let object = db.read_object(&entry.oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            entry.oid,
            object.object_type.as_str()
        )));
    }
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(file_path, &object.body)?;
    Ok(())
}

fn set_skip_worktree(entry: &mut IndexEntry) {
    entry.flags |= INDEX_FLAG_EXTENDED;
    entry.flags_extended |= INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
}

fn clear_skip_worktree(entry: &mut IndexEntry) {
    entry.flags_extended &= !INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
    if entry.flags_extended == 0 {
        entry.flags &= !INDEX_FLAG_EXTENDED;
    }
}

pub fn restore_worktree_paths_from_head(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    restore_worktree_paths_from_entries(worktree_root, &db, index, &head_entries, paths)
}

pub fn restore_worktree_paths_from_tree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let source_entries = tree_entries(&db, format, tree_oid)?;
    restore_worktree_paths_from_entries(worktree_root, &db, index, &source_entries, paths)
}

fn restore_worktree_paths_from_entries(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let index_entries = index
        .entries
        .into_iter()
        .map(|entry| entry.path.into_bytes())
        .collect::<BTreeSet<_>>();
    let mut restored = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        let recursive = path == Path::new(".")
            || path.to_string_lossy().ends_with('/')
            || absolute.is_dir()
            || index_entries
                .iter()
                .any(|entry| index_entry_is_under_path(entry, &git_path))
            || source_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path));
        let mut matched_paths = BTreeSet::new();
        for path in index_entries.iter().chain(source_entries.keys()) {
            if path.as_slice() == git_path.as_slice()
                || (recursive && index_entry_is_under_path(path, &git_path))
            {
                matched_paths.insert(path.clone());
            }
        }
        if matched_paths.is_empty() {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                path.display()
            );
            return Err(GitError::Exit(1));
        }
        for path in matched_paths {
            if let Some(entry) = source_entries.get(&path) {
                restore_head_entry_to_worktree(worktree_root, db, &path, entry)?;
            } else {
                remove_worktree_file(worktree_root, &path)?;
            }
            restored.insert(path);
        }
    }
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn remove_index_and_worktree_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: RemoveOptions,
) -> Result<RemoveResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    let mut index_entries = index
        .entries
        .into_iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if index_entries.contains_key(&git_path) {
            selected.insert(git_path);
            continue;
        }
        let matched = index_entries
            .keys()
            .filter(|entry| index_entry_is_under_path(entry, &git_path))
            .cloned()
            .collect::<Vec<_>>();
        if matched.is_empty() {
            if options.ignore_unmatch {
                continue;
            }
            eprintln!(
                "fatal: pathspec '{}' did not match any files",
                String::from_utf8_lossy(&git_path)
            );
            return Err(GitError::Exit(128));
        }
        if !options.recursive {
            eprintln!(
                "fatal: not removing '{}' recursively without -r",
                String::from_utf8_lossy(&git_path)
            );
            return Err(GitError::Exit(128));
        }
        selected.extend(matched);
    }
    if !options.cached && !options.force {
        let config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
        for path in &selected {
            let Some(index_entry) = index_entries.get(path) else {
                continue;
            };
            match head_entries.get(path) {
                Some(head_entry)
                    if head_entry.oid == index_entry.oid && head_entry.mode == index_entry.mode => {
                }
                _ => {
                    eprintln!("error: the following file has changes staged in the index:");
                    eprintln!("    {}", String::from_utf8_lossy(path));
                    eprintln!("(use --cached to keep the file, or -f to force removal)");
                    return Err(GitError::Exit(1));
                }
            }
            let worktree_file = worktree_path(worktree_root, path)?;
            if worktree_file.exists() {
                let object = db.read_object(&index_entry.oid)?;
                if object.object_type != ObjectType::Blob {
                    return Err(GitError::InvalidObject(format!(
                        "expected blob {}, found {}",
                        index_entry.oid,
                        object.object_type.as_str()
                    )));
                }
                let worktree_bytes = apply_clean_filter(
                    worktree_root,
                    git_dir,
                    &config,
                    path,
                    &fs::read(&worktree_file)?,
                )?;
                if worktree_bytes != object.body {
                    eprintln!("error: the following file has local modifications:");
                    eprintln!("    {}", String::from_utf8_lossy(path));
                    eprintln!("(use --cached to keep the file, or -f to force removal)");
                    return Err(GitError::Exit(1));
                }
            }
        }
    }
    for path in &selected {
        if options.dry_run {
            continue;
        }
        if !options.cached {
            remove_worktree_file(worktree_root, path)?;
        }
        index_entries.remove(path);
    }
    if options.dry_run {
        return Ok(RemoveResult {
            removed: selected.into_iter().collect(),
        });
    }
    let entries = index_entries.into_values().collect::<Vec<_>>();
    fs::write(
        index_path,
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RemoveResult {
        removed: selected.into_iter().collect(),
    })
}

pub fn move_index_and_worktree_path(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    source: &Path,
    destination: &Path,
    options: MoveOptions,
) -> Result<MoveResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let source_absolute = if source.is_absolute() {
        source.to_path_buf()
    } else {
        worktree_root.join(source)
    };
    let destination_absolute = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        worktree_root.join(destination)
    };
    let destination_absolute = if destination_absolute.is_dir() {
        let Some(file_name) = source_absolute.file_name() else {
            return Err(GitError::InvalidPath(format!(
                "invalid source path {}",
                source.display()
            )));
        };
        destination_absolute.join(file_name)
    } else {
        destination_absolute
    };
    let source_relative = source_absolute.strip_prefix(worktree_root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", source.display()))
    })?;
    let destination_relative = destination_absolute
        .strip_prefix(worktree_root)
        .map_err(|_| {
            GitError::InvalidPath(format!(
                "path {} is outside worktree",
                destination.display()
            ))
        })?;
    let source_path = git_path_bytes(source_relative)?;
    let destination_path = git_path_bytes(destination_relative)?;
    let destination_has_trailing_separator = path_has_trailing_separator(&destination_absolute);
    if destination_has_trailing_separator && !destination_absolute.is_dir() {
        if options.skip_errors {
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: true,
                fatal: None,
                details: Vec::new(),
            });
        }
        let mut destination = String::from_utf8_lossy(&destination_path).into_owned();
        destination.push('/');
        if options.dry_run {
            let fatal = format!(
                "fatal: destination directory does not exist, source={}, destination={destination}",
                String::from_utf8_lossy(&source_path),
            );
            return Ok(MoveResult {
                source: source_path,
                destination: destination.clone().into_bytes(),
                skipped: false,
                fatal: Some(fatal),
                details: Vec::new(),
            });
        }
        eprintln!(
            "fatal: destination directory does not exist, source={}, destination={destination}",
            String::from_utf8_lossy(&source_path),
        );
        return Err(GitError::Exit(128));
    }
    if destination_absolute.exists() {
        if !options.force {
            if options.skip_errors {
                return Ok(MoveResult {
                    source: source_path,
                    destination: destination_path,
                    skipped: true,
                    fatal: None,
                    details: Vec::new(),
                });
            }
            if options.dry_run {
                let fatal = format!(
                    "fatal: destination exists, source={}, destination={}",
                    String::from_utf8_lossy(&source_path),
                    String::from_utf8_lossy(&destination_path)
                );
                return Ok(MoveResult {
                    source: source_path,
                    destination: destination_path,
                    skipped: false,
                    fatal: Some(fatal),
                    details: Vec::new(),
                });
            }
            eprintln!(
                "fatal: destination exists, source={}, destination={}",
                String::from_utf8_lossy(&source_path),
                String::from_utf8_lossy(&destination_path)
            );
            return Err(GitError::Exit(128));
        }
        if !options.dry_run && destination_absolute.is_dir() {
            fs::remove_dir_all(&destination_absolute)?;
        } else if !options.dry_run {
            fs::remove_file(&destination_absolute)?;
        }
    }
    let directory_prefix = {
        let mut prefix = source_path.clone();
        prefix.push(b'/');
        prefix
    };
    let directory_entries: Vec<_> = index
        .entries
        .iter()
        .filter(|entry| entry.path.as_bytes().starts_with(&directory_prefix))
        .cloned()
        .collect();
    if !directory_entries.is_empty() {
        let details: Vec<_> = directory_entries
            .iter()
            .map(|entry| {
                let suffix = &entry.path.as_bytes()[source_path.len()..];
                let mut destination = destination_path.clone();
                destination.extend_from_slice(suffix);
                MoveDetail {
                    source: entry.path.as_bytes().to_vec(),
                    destination,
                    skipped: false,
                }
            })
            .collect();
        if options.dry_run {
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: false,
                fatal: None,
                details,
            });
        }
        fs::rename(&source_absolute, &destination_absolute)?;
        let moved_paths: Vec<_> = details
            .iter()
            .map(|detail| detail.destination.clone())
            .collect();
        index.entries.retain(|entry| {
            !entry.path.as_bytes().starts_with(&directory_prefix)
                && !moved_paths
                    .iter()
                    .any(|m| m.as_slice() == entry.path.as_bytes())
        });
        for (source_entry, detail) in directory_entries.into_iter().zip(details.iter()) {
            let relative_path = git_path_to_relative_path(&detail.destination)?;
            let metadata = fs::metadata(worktree_root.join(relative_path))?;
            let mut destination_entry =
                index_entry_from_metadata(detail.destination.clone(), source_entry.oid, &metadata);
            destination_entry.mode = source_entry.mode;
            index.entries.push(destination_entry);
        }
        index
            .entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        index.extensions.clear();
        fs::write(index_path, index.write(format)?)?;
        return Ok(MoveResult {
            source: source_path,
            destination: destination_path,
            skipped: false,
            fatal: None,
            details,
        });
    }

    let Some(position) = index
        .entries
        .iter()
        .position(|entry| entry.path == source_path)
    else {
        if options.skip_errors {
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: true,
                fatal: None,
                details: Vec::new(),
            });
        }
        let source_kind = if source_absolute.exists() {
            "not under version control"
        } else {
            "bad source"
        };
        if options.dry_run {
            let fatal = format!(
                "fatal: {source_kind}, source={}, destination={}",
                String::from_utf8_lossy(&source_path),
                String::from_utf8_lossy(&destination_path)
            );
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: false,
                fatal: Some(fatal),
                details: Vec::new(),
            });
        }
        eprintln!(
            "fatal: {source_kind}, source={}, destination={}",
            String::from_utf8_lossy(&source_path),
            String::from_utf8_lossy(&destination_path)
        );
        return Err(GitError::Exit(128));
    };
    if options.dry_run {
        return Ok(MoveResult {
            source: source_path,
            destination: destination_path,
            skipped: false,
            fatal: None,
            details: Vec::new(),
        });
    }
    if let Some(parent) = destination_absolute.parent()
        && !parent.exists()
    {
        if options.skip_errors {
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: true,
                fatal: None,
                details: Vec::new(),
            });
        }
        eprintln!(
            "fatal: renaming '{}' failed: No such file or directory",
            String::from_utf8_lossy(&source_path)
        );
        return Err(GitError::Exit(128));
    }
    fs::rename(&source_absolute, &destination_absolute)?;
    let metadata = fs::metadata(&destination_absolute)?;
    let source_entry = index.entries.remove(position);
    let mut destination_entry =
        index_entry_from_metadata(destination_path.clone(), source_entry.oid, &metadata);
    destination_entry.mode = source_entry.mode;
    index.entries.retain(|entry| entry.path != destination_path);
    index.entries.push(destination_entry);
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    index.extensions.clear();
    fs::write(index_path, index.write(format)?)?;
    Ok(MoveResult {
        source: source_path,
        destination: destination_path,
        skipped: false,
        fatal: None,
        details: Vec::new(),
    })
}

fn restore_index_entry(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    entry: &IndexEntry,
) -> Result<()> {
    let object = db.read_object(&entry.oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            entry.oid,
            object.object_type.as_str()
        )));
    }
    let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(file_path, &object.body)?;
    Ok(())
}

fn restored_head_index_entry(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<IndexEntry> {
    let file_path = worktree_path(worktree_root, path)?;
    // This restores the index from a tree (reset --mixed / stash / sparse) WITHOUT
    // rewriting the worktree file, so the file on disk may hold different content
    // than `entry.oid`. Crucially we must NOT copy the worktree file's stat onto
    // this entry: that would make the cached stat match a file whose real content
    // hashes to a DIFFERENT oid, breaking git's "stat-match implies oid-match"
    // invariant that the status stat-cache relies on. Leave the stat zeroed so
    // status always re-hashes this path and detects any modification -- exactly
    // git's behavior for tree-sourced entries until a later refresh validates them.
    let size = match fs::metadata(&file_path) {
        Ok(metadata) => metadata.len().min(u32::MAX as u64) as u32,
        Err(_) => {
            let object = db.read_object(&entry.oid)?;
            if object.object_type != ObjectType::Blob {
                return Err(GitError::InvalidObject(format!(
                    "expected blob {}, found {}",
                    entry.oid,
                    object.object_type.as_str()
                )));
            }
            object.body.len().min(u32::MAX as usize) as u32
        }
    };
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
        size,
        oid: entry.oid,
        flags: path.len().min(0x0fff) as u16,
        flags_extended: 0,
        path: BString::from(path),
    })
}

fn restore_head_entry_to_worktree(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<()> {
    let object = db.read_object(&entry.oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            entry.oid,
            object.object_type.as_str()
        )));
    }
    let file_path = worktree_path(worktree_root, path)?;
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(file_path, &object.body)?;
    Ok(())
}

fn restore_head_entry_to_worktree_and_index(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<IndexEntry> {
    let object = db.read_object(&entry.oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            entry.oid,
            object.object_type.as_str()
        )));
    }
    let file_path = worktree_path(worktree_root, path)?;
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&file_path, &object.body)?;
    let metadata = fs::metadata(&file_path)?;
    let mut index_entry = index_entry_from_metadata(path.to_vec(), entry.oid, &metadata);
    index_entry.mode = entry.mode;
    Ok(index_entry)
}

fn index_has_entry_under(entries: &[IndexEntry], directory: &[u8]) -> bool {
    entries
        .iter()
        .any(|entry| index_entry_is_under_path(entry.path.as_bytes(), directory))
}

fn index_entry_is_under_path(entry_path: &[u8], directory: &[u8]) -> bool {
    if directory.is_empty() {
        return true;
    }
    entry_path
        .strip_prefix(directory)
        .and_then(|rest| rest.strip_prefix(b"/"))
        .is_some()
}

fn index_entry_from_metadata(
    path: impl Into<BString>,
    oid: ObjectId,
    metadata: &fs::Metadata,
) -> IndexEntry {
    let modified = metadata.modified().ok();
    let duration = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    let mode = file_mode(metadata);
    let path = path.into();
    let flags = path.len().min(0x0fff) as u16;
    IndexEntry {
        ctime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        ctime_nanoseconds: duration.subsec_nanos(),
        mtime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        mtime_nanoseconds: duration.subsec_nanos(),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: metadata.len().min(u32::MAX as u64) as u32,
        oid,
        flags,
        flags_extended: 0,
        path,
    }
}

fn read_commit(db: &FileObjectDatabase, format: ObjectFormat, oid: &ObjectId) -> Result<Commit> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Commit::parse(format, &object.body)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedEntry {
    mode: u32,
    oid: ObjectId,
}

/// git's racy-git stat cache: the stage-0 index entries keyed by path (so the
/// worktree walk can reuse a cached oid when a file's stat shows it is unchanged
/// since it was staged) plus the index *file's* own mtime, which git uses as the
/// racy-clean reference timestamp.
///
/// SAFETY INVARIANT: trusting a cached oid by stat alone is only sound because
/// every code path that stamps a worktree stat onto an index entry also hashed
/// that exact file content (see `index_entry_from_metadata`), while tree-sourced
/// restores (reset --mixed / stash / sparse) leave the stat zeroed
/// (`restored_head_index_entry`). So a non-zero, non-racy stat match implies the
/// cached oid is the file's true content. When that does not hold we fall through
/// to a full read+filter+hash, so a modified file is never reported clean.
#[derive(Debug, Clone, Default)]
struct IndexStatCache {
    entries: BTreeMap<Vec<u8>, IndexEntry>,
    /// The index file's modification time as `(seconds, nanoseconds)`, or `None`
    /// when it could not be determined. Used as git's racy-clean reference.
    index_mtime: Option<(u64, u64)>,
}

impl IndexStatCache {
    /// Builds the cache from an already-parsed index plus the path of the index
    /// file on disk (whose mtime becomes the racy-clean reference). Only stage-0
    /// entries are retained; higher merge stages never describe a worktree file.
    fn from_index(index: &Index, index_path: &Path) -> Self {
        let mut entries = BTreeMap::new();
        for entry in &index.entries {
            if index_entry_stage(entry) != 0 {
                continue;
            }
            entries.insert(entry.path.as_bytes().to_vec(), entry.clone());
        }
        let index_mtime = fs::metadata(index_path)
            .ok()
            .and_then(|metadata| file_mtime_parts(&metadata));
        IndexStatCache {
            entries,
            index_mtime,
        }
    }

    /// Whether `entry` is "racily clean" in git's sense: its cached mtime is not
    /// strictly older than the index file's mtime, so a same-timestamp write
    /// could have changed the content without moving the stat. Such entries must
    /// always be re-hashed.
    ///
    /// Conservative by construction: if the index mtime is unknown, or either
    /// side's mtime is zero (e.g. a tree-sourced entry whose stat was left
    /// zeroed), this returns `true` so the caller re-hashes rather than trusting
    /// a stat we cannot prove safe.
    fn is_racily_clean(&self, entry: &IndexEntry) -> bool {
        let Some(index_mtime) = self.index_mtime else {
            return true;
        };
        if index_mtime == (0, 0) {
            return true;
        }
        let entry_mtime = (
            u64::from(entry.mtime_seconds),
            u64::from(entry.mtime_nanoseconds),
        );
        if entry_mtime == (0, 0) {
            return true;
        }
        // Racy unless the index was written strictly after the entry's mtime.
        index_mtime <= entry_mtime
    }

    /// Whether the index has a stage-0 entry for `git_path` (i.e. the path is
    /// tracked). Used to skip hashing untracked worktree files.
    fn contains(&self, git_path: &[u8]) -> bool {
        self.entries.contains_key(git_path)
    }

    /// Returns the cached [`TrackedEntry`] for `git_path` (reusing its stored
    /// oid, so the caller can SKIP reading, filtering, and hashing the file) only
    /// when the worktree file is provably unchanged since it was staged: a
    /// stage-0 entry exists, its recorded mode matches the file's current mode
    /// (catching pure `chmod`s that do not move mtime), the size+mtime stat
    /// check passes, and the entry is not racily clean. Otherwise returns `None`
    /// and the caller hashes the file as usual.
    fn reuse_tracked_entry(
        &self,
        git_path: &[u8],
        worktree_metadata: &fs::Metadata,
    ) -> Option<TrackedEntry> {
        let entry = self.entries.get(git_path)?;
        if entry.mode != worktree_entry_mode(worktree_metadata) {
            return None;
        }
        if !worktree_entry_is_uptodate(entry, worktree_metadata) {
            return None;
        }
        if self.is_racily_clean(entry) {
            return None;
        }
        Some(TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        })
    }
}

fn read_index_entries(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    Ok(read_index_entries_with_stat_cache(git_dir, format, &db)?.0)
}

fn resolve_head_tree_oid(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<Option<ObjectId>> {
    let Some(commit_oid) = resolve_head_commit_oid(git_dir, format)? else {
        return Ok(None);
    };
    let object = db.read_object(&commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "HEAD {commit_oid} is not a commit"
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    Ok(Some(commit.tree))
}

fn resolve_head_commit_oid(git_dir: &Path, format: ObjectFormat) -> Result<Option<ObjectId>> {
    let refs = FileRefStore::new(git_dir, format);
    sley_refs::resolve_ref_peeled(&refs, "HEAD")
}

fn status_entry_is_untracked_or_ignored(entry: &ShortStatusEntry) -> bool {
    matches!((entry.index, entry.worktree), (b'?', b'?') | (b'!', b'!'))
}

fn checkout_switch_head_symbolic(
    refs: &FileRefStore,
    branch_ref: String,
    committer: Vec<u8>,
    branch: &str,
    old_oid: Option<ObjectId>,
    new_oid: Option<ObjectId>,
) -> Result<()> {
    let mut tx = refs.transaction();
    let reflog = match (old_oid, new_oid) {
        (Some(old_oid), Some(new_oid)) => Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: format!("checkout: moving from HEAD to {branch}").into_bytes(),
        }),
        _ => None,
    };
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(branch_ref),
        reflog,
    });
    tx.commit()
}

fn cache_tree_is_valid(tree: &CacheTree) -> bool {
    if tree.entry_count < 0 || tree.oid.is_none() {
        return false;
    }
    tree.subtrees
        .iter()
        .all(|child| cache_tree_is_valid(&child.tree))
}

fn index_stage0_entry_count(index: &Index) -> usize {
    index
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .count()
}

fn head_matches_index_from_cache_tree(
    index: &Index,
    format: ObjectFormat,
    head_tree_oid: &ObjectId,
) -> Result<bool> {
    let Some(cache_tree) = index.cache_tree(format)? else {
        return Ok(false);
    };
    if !cache_tree_is_valid(&cache_tree) {
        return Ok(false);
    }
    let Some(root_oid) = cache_tree.oid.as_ref() else {
        return Ok(false);
    };
    if root_oid != head_tree_oid {
        return Ok(false);
    }
    Ok(cache_tree.entry_count as usize == index_stage0_entry_count(index))
}

/// Parses the index a single time and returns both the path -> [`TrackedEntry`]
/// map used for status comparisons AND the [`IndexStatCache`] used to short-cut
/// the worktree walk, avoiding a second parse of the same file.
fn read_index_entries_with_stat_cache(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<(BTreeMap<Vec<u8>, TrackedEntry>, IndexStatCache, bool)> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok((BTreeMap::new(), IndexStatCache::default(), false));
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    let head_matches_index = match resolve_head_tree_oid(git_dir, format, db)? {
        Some(head_tree_oid) => head_matches_index_from_cache_tree(&index, format, &head_tree_oid)?,
        None => false,
    };
    let stat_cache = IndexStatCache::from_index(&index, &index_path);
    let tracked = index
        .entries
        .into_iter()
        .map(|entry| {
            (
                entry.path.into_bytes(),
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            )
        })
        .collect();
    Ok((tracked, stat_cache, head_matches_index))
}

fn head_tree_entries(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let refs = FileRefStore::new(git_dir, format);
    let Some(head) = refs.read_ref("HEAD")? else {
        return Ok(BTreeMap::new());
    };
    let commit_oid = match head {
        RefTarget::Direct(oid) => Some(oid),
        RefTarget::Symbolic(name) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
    };
    let Some(commit_oid) = commit_oid else {
        return Ok(BTreeMap::new());
    };
    let object = db.read_object(&commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "HEAD {commit_oid} is not a commit"
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, &commit.tree, &mut entries)?;
    Ok(entries)
}

fn tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, tree_oid, &mut entries)?;
    Ok(entries)
}

fn collect_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    let mut path = Vec::new();
    collect_tree_entries_into(db, format, tree_oid, &mut path, entries)
}

fn collect_tree_entries_into(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    path: &mut Vec<u8>,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let original_len = path.len();
        if original_len != 0 {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            collect_tree_entries_into(db, format, &entry.oid, path, entries)?;
        } else {
            entries.insert(
                path.clone(),
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            );
        }
        path.truncate(original_len);
    }
    Ok(())
}

/// Like a full worktree walk, but accepts the index's [`IndexStatCache`] so the
/// walk can reuse a cached oid for files that are provably unchanged since they
/// were staged, skipping the read+filter+hash for those paths. Passing `None`
/// hashes every file when no stat cache is supplied.
fn worktree_entries_with_stat_cache(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    stat_cache: Option<&IndexStatCache>,
    tracked_paths: Option<&BTreeSet<Vec<u8>>>,
    ignores: Option<&mut IgnoreMatcher>,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    // Worktree blobs are compared to the index by OID, so they must be passed
    // through the clean filter (core.autocrlf / .gitattributes) first -- exactly
    // as `git add` would store them. With no filter configured this is an exact
    // passthrough, so unfiltered repositories see identical OIDs.
    let config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    // Seed the matcher with the repo-wide sources only; each directory's
    // `.gitattributes` is folded in by `collect_worktree_entries` as it descends,
    // so the worktree is read exactly once (a separate full-tree attribute pass was
    // a second traversal of every directory).
    let mut attr_matcher = AttributeMatcher::from_worktree_base(worktree_root);
    let attr_requested = filter_attribute_names();
    let mut context = WorktreeEntriesWalk {
        root: worktree_root,
        git_dir,
        format,
        config: &config,
        matcher: &mut attr_matcher,
        requested: &attr_requested,
        stat_cache,
        tracked_paths,
        ignores,
        entries: &mut entries,
    };
    collect_worktree_entries(&mut context, worktree_root)?;
    Ok(entries)
}

struct WorktreeEntriesWalk<'a> {
    root: &'a Path,
    git_dir: &'a Path,
    format: ObjectFormat,
    config: &'a GitConfig,
    matcher: &'a mut AttributeMatcher,
    requested: &'a [Vec<u8>],
    stat_cache: Option<&'a IndexStatCache>,
    tracked_paths: Option<&'a BTreeSet<Vec<u8>>>,
    ignores: Option<&'a mut IgnoreMatcher>,
    entries: &'a mut BTreeMap<Vec<u8>, TrackedEntry>,
}

fn collect_worktree_entries(context: &mut WorktreeEntriesWalk<'_>, dir: &Path) -> Result<()> {
    if is_same_path(dir, context.git_dir) {
        return Ok(());
    }
    // Fold this directory's `.gitattributes` into the matcher before processing its
    // files, so lookups for files here (and below) see it. This is what lets the
    // walk read the tree once instead of doing a separate full-tree attribute pass.
    read_dir_attribute_patterns(context.root, dir, context.matcher)?;
    if let Some(ignores) = context.ignores.as_deref_mut() {
        read_dir_ignore_patterns(context.root, dir, ignores)?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if is_worktree_dot_git(context.root, &path) {
            continue;
        }
        if is_embedded_git_internals(context.root, &path) {
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
        if context
            .ignores
            .as_ref()
            .is_some_and(|ignores| ignores.is_ignored(&git_path, metadata.is_dir()))
        {
            if metadata.is_dir()
                && context.tracked_paths.is_some_and(|tracked_paths| {
                    tracked_paths_may_contain(tracked_paths, &git_path)
                })
            {
                collect_worktree_entries(context, &path)?;
            }
            continue;
        }
        if metadata.is_dir() {
            if is_nested_repository_boundary(&path) {
                if let Some(tracked_paths) = context.tracked_paths
                    && !tracked_paths_may_contain(tracked_paths, &git_path)
                {
                    continue;
                }
                context.entries.insert(
                    git_path,
                    TrackedEntry {
                        mode: 0o040000,
                        oid: ObjectId::null(context.format),
                    },
                );
                continue;
            }
            if let Some(tracked_paths) = context.tracked_paths
                && !tracked_paths_may_contain(tracked_paths, &git_path)
            {
                continue;
            }
            collect_worktree_entries(context, &path)?;
        } else if metadata.is_file() || metadata.file_type().is_symlink() {
            if let Some(tracked_paths) = context.tracked_paths
                && !tracked_paths.contains(&git_path)
            {
                continue;
            }
            let entry_mode = worktree_entry_mode(&metadata);
            // git's racy-git stat shortcut: when the index's cached stat proves
            // this file is unchanged since it was staged, reuse the staged oid
            // and skip the read+filter+hash entirely. `reuse_tracked_entry`
            // returns `Some` ONLY for a non-racy size+mtime+mode match, so a
            // modified file always falls through to the full hash below and is
            // never silently reported clean.
            if let Some(tracked) = context
                .stat_cache
                .and_then(|cache| cache.reuse_tracked_entry(&git_path, &metadata))
            {
                context.entries.insert(git_path, tracked);
                continue;
            }
            // A file absent from the index is untracked: status and the
            // index-vs-worktree diff report it by *presence* (`??` / nothing), never
            // by content, so computing its oid is wasted work — git never hashes
            // untracked files. Record presence with a null oid and skip the
            // read+filter+hash. Without a stat cache we cannot tell tracked from
            // untracked, so fall through and hash as before.
            if context
                .stat_cache
                .is_some_and(|cache| !cache.contains(&git_path))
            {
                context.entries.insert(
                    git_path,
                    TrackedEntry {
                        mode: entry_mode,
                        oid: ObjectId::null(context.format),
                    },
                );
                continue;
            }
            let body = fs::read(&path)?;
            // Resolve this path's attributes against the prebuilt matcher (a cheap
            // pattern match) and apply the clean filter -- no per-file matcher
            // rebuild. With no attributes/autocrlf configured this is an exact
            // passthrough, so the stored OID is unchanged.
            let checks = context
                .matcher
                .attributes_for_path(&git_path, context.requested, false);
            let body =
                apply_clean_filter_with_attributes(context.config, &checks, &git_path, &body)?;
            let oid = EncodedObject::new(ObjectType::Blob, body).object_id(context.format)?;
            context.entries.insert(
                git_path,
                TrackedEntry {
                    mode: entry_mode,
                    oid,
                },
            );
        }
    }
    Ok(())
}

fn tracked_paths_may_contain(tracked_paths: &BTreeSet<Vec<u8>>, directory: &[u8]) -> bool {
    if tracked_paths.contains(directory) {
        return true;
    }
    let mut prefix = Vec::with_capacity(directory.len() + 1);
    prefix.extend_from_slice(directory);
    prefix.push(b'/');
    tracked_paths
        .range::<[u8], _>((
            std::ops::Bound::Included(prefix.as_slice()),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .is_some_and(|path| path.starts_with(&prefix))
}

fn is_same_path(left: &Path, right: &Path) -> bool {
    left == right
}

fn is_worktree_dot_git(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|relative| relative == Path::new(".git"))
}

/// Whether `path` is a directory that contains a `.git` gitfile or embedded git dir.
fn is_nested_repository_boundary(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Whether `path` is an embedded repository's `.git` directory or a path inside it.
fn is_embedded_git_internals(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if matches!(component, std::path::Component::Normal(name) if name == ".git")
            && current != root
            && current.join(".git").is_dir()
        {
            return true;
        }
        current.push(component);
    }
    false
}

fn worktree_entry_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        0o120000
    } else if metadata.is_dir() {
        0o040000
    } else {
        file_mode(metadata)
    }
}

fn worktree_path(root: &Path, path: &[u8]) -> Result<PathBuf> {
    let text = std::str::from_utf8(path).map_err(|err| GitError::InvalidPath(err.to_string()))?;
    let relative = PathBuf::from(text);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(GitError::InvalidPath(format!(
            "invalid worktree path {text}"
        )));
    }
    Ok(root.join(relative))
}

fn remove_worktree_file(root: &Path, path: &[u8]) -> Result<()> {
    let file = worktree_path(root, path)?;
    if file.exists() {
        fs::remove_file(&file)?;
        prune_empty_parents(root, file.parent())?;
    }
    Ok(())
}

fn prune_empty_parents(root: &Path, mut dir: Option<&Path>) -> Result<()> {
    while let Some(path) = dir {
        if path == root {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => dir = path.parent(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => dir = path.parent(),
            Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct TreeNode {
    files: Vec<TreeFile>,
    directories: BTreeMap<Vec<u8>, TreeNode>,
}

#[derive(Debug)]
struct TreeFile {
    name: Vec<u8>,
    mode: u32,
    oid: ObjectId,
}

impl TreeNode {
    fn insert(&mut self, entry: &IndexEntry) -> Result<()> {
        let components = entry
            .path
            .as_bytes()
            .split(|byte| *byte == b'/')
            .collect::<Vec<_>>();
        if components.iter().any(|component| component.is_empty()) {
            return Err(GitError::InvalidPath(format!(
                "invalid index path {}",
                String::from_utf8_lossy(entry.path.as_bytes())
            )));
        }
        self.insert_components(&components, entry)
    }

    fn insert_components(&mut self, components: &[&[u8]], entry: &IndexEntry) -> Result<()> {
        match components {
            [] => Err(GitError::InvalidPath("empty index path".into())),
            [name] => {
                self.files.push(TreeFile {
                    name: name.to_vec(),
                    mode: entry.mode,
                    oid: entry.oid,
                });
                Ok(())
            }
            [directory, rest @ ..] => self
                .directories
                .entry(directory.to_vec())
                .or_default()
                .insert_components(rest, entry),
        }
    }
}

fn write_tree_node(node: &TreeNode, odb: &mut FileObjectDatabase) -> Result<ObjectId> {
    let mut entries = Vec::with_capacity(node.files.len() + node.directories.len());
    for file in &node.files {
        entries.push(TreeEntry {
            mode: file.mode,
            name: BString::from(file.name.as_slice()),
            oid: file.oid,
        });
    }
    for (name, child) in &node.directories {
        let oid = write_tree_node(child, odb)?;
        entries.push(TreeEntry {
            mode: 0o040000,
            name: BString::from(name.as_slice()),
            oid,
        });
    }
    entries.sort_by(|left, right| {
        git_tree_entry_cmp(
            left.name.as_bytes(),
            left.mode,
            right.name.as_bytes(),
            right.mode,
        )
    });
    odb.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree { entries }.write(),
    ))
}

fn git_tree_entry_cmp(
    left_name: &[u8],
    left_mode: u32,
    right_name: &[u8],
    right_mode: u32,
) -> Ordering {
    let shared = left_name.len().min(right_name.len());
    let name_order = left_name[..shared].cmp(&right_name[..shared]);
    if name_order != Ordering::Equal {
        return name_order;
    }
    let left_end = left_name.len() == shared;
    let right_end = right_name.len() == shared;
    match (left_end, right_end) {
        (true, true) => Ordering::Equal,
        (true, false) => tree_name_terminator(left_mode).cmp(&right_name[shared]),
        (false, true) => left_name[shared].cmp(&tree_name_terminator(right_mode)),
        (false, false) => Ordering::Equal,
    }
}

fn tree_name_terminator(mode: u32) -> u8 {
    if mode == 0o040000 { b'/' } else { 0 }
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

fn git_path_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(GitError::InvalidPath(format!(
            "invalid index path {}",
            path.display()
        )));
    }
    Ok(path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes())
}

fn repo_path_to_os_path(path: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidPath("index path is not utf8".into()))?;
    Ok(path.split('/').collect())
}

fn git_path_to_relative_path(path: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(path)
        .map_err(|err| GitError::InvalidPath(format!("invalid utf-8 index path: {err}")))?;
    Ok(path.split('/').collect())
}

fn path_has_trailing_separator(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .ends_with(std::path::MAIN_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_odb::ObjectReader;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Build an in-memory ignore matcher from raw `.gitignore` lines (no disk).
    fn ignore_matcher(patterns: &[&[u8]]) -> IgnoreMatcher {
        let mut matcher = IgnoreMatcher::default();
        let owned: Vec<Vec<u8>> = patterns.iter().map(|p| p.to_vec()).collect();
        matcher.extend_patterns(&owned);
        matcher
    }

    #[test]
    fn ignore_match_kind_fast_paths_match_the_wildcard_engine() {
        // Literal: exact basename anywhere; not a superstring.
        let matcher = ignore_matcher(&[b"Pods"]);
        assert!(matcher.is_ignored(b"a/b/Pods", true));
        assert!(matcher.is_ignored(b"Pods", false));
        assert!(!matcher.is_ignored(b"Pods_not", false));
        assert!(matches!(
            classify_ignore_pattern(b"Pods"),
            MatchKind::Literal
        ));

        // Suffix `*.log`: basename ending in `.log` at any depth.
        let matcher = ignore_matcher(&[b"*.log"]);
        assert!(matcher.is_ignored(b"x.log", false));
        assert!(matcher.is_ignored(b"a/b/x.log", false));
        assert!(matcher.is_ignored(b".log", false));
        assert!(!matcher.is_ignored(b"x.logx", false));
        assert!(matches!(
            classify_ignore_pattern(b"*.log"),
            MatchKind::Suffix
        ));

        // Prefix `build*`: basename starting with `build`.
        let matcher = ignore_matcher(&[b"build*"]);
        assert!(matcher.is_ignored(b"buildfoo", false));
        assert!(matcher.is_ignored(b"a/build", false));
        assert!(!matcher.is_ignored(b"xbuild", false));
        assert!(matches!(
            classify_ignore_pattern(b"build*"),
            MatchKind::Prefix
        ));
    }

    #[test]
    fn ignore_anchored_suffix_does_not_cross_slash() {
        // `/*.log` is anchored: matches `.log` files only at the matcher base,
        // never in a subdirectory — the slash guard in `match_segment`.
        let matcher = ignore_matcher(&[b"/*.log"]);
        assert!(matcher.is_ignored(b"x.log", false));
        assert!(!matcher.is_ignored(b"sub/x.log", false));

        // Anchored literal likewise only matches at root.
        let matcher = ignore_matcher(&[b"/foo"]);
        assert!(matcher.is_ignored(b"foo", false));
        assert!(!matcher.is_ignored(b"a/foo", false));
    }

    #[test]
    fn ignore_double_star_prefix_collapses_to_basename() {
        // `**/X` ≡ `X` for slash-free X (verified against `git check-ignore`).
        let matcher = ignore_matcher(&[b"**/Pods"]);
        assert!(matcher.is_ignored(b"a/b/Pods", true));
        assert!(matcher.is_ignored(b"Pods", true));
        assert!(!matcher.is_ignored(b"Pods_not", false));

        let matcher = ignore_matcher(&[b"**/*.jks"]);
        assert!(matcher.is_ignored(b"x.jks", false));
        assert!(matcher.is_ignored(b"a/deep/y.jks", false));
        assert!(!matcher.is_ignored(b"x.jksx", false));

        // `**/A/B` keeps a slash in the tail, so it stays a real glob and must
        // match the trailing path at any depth.
        let matcher = ignore_matcher(&[b"**/Flutter/ephemeral"]);
        assert!(matcher.is_ignored(b"Flutter/ephemeral", true));
        assert!(matcher.is_ignored(b"a/Flutter/ephemeral", true));
        assert!(!matcher.is_ignored(b"Flutter/other", true));
    }

    #[test]
    fn ignore_complex_globs_still_use_the_engine() {
        let matcher = ignore_matcher(&[b"*.[Cc]ache"]);
        assert!(matcher.is_ignored(b"x.cache", false));
        assert!(matcher.is_ignored(b"x.Cache", false));
        assert!(!matcher.is_ignored(b"x.xache", false));
        assert!(matches!(
            classify_ignore_pattern(b"*.[Cc]ache"),
            MatchKind::Glob
        ));

        let matcher = ignore_matcher(&[b"Icon?"]);
        assert!(matcher.is_ignored(b"IconA", false));
        assert!(!matcher.is_ignored(b"Icon", false));
        assert!(!matcher.is_ignored(b"IconAB", false));

        // Multi-star is not a simple prefix/suffix.
        assert!(matches!(
            classify_ignore_pattern(b"app.*.symbols"),
            MatchKind::Glob
        ));
        assert!(matches!(classify_ignore_pattern(b"a*b*c"), MatchKind::Glob));
    }

    #[test]
    fn ignore_negation_still_applies_after_fast_paths() {
        // Last match wins: a negated literal un-ignores a suffix-matched file.
        let matcher = ignore_matcher(&[b"*.log", b"!keep.log"]);
        assert!(matcher.is_ignored(b"a/x.log", false));
        assert!(!matcher.is_ignored(b"a/keep.log", false));
    }

    #[test]
    fn update_index_adds_file_entry_and_blob() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 1);
        let index = Index::parse_v2_sha1(
            &fs::read(repository_index_path(git_dir)).expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        assert_eq!(index.entries[0].path, b"hello.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn update_index_and_write_tree_support_sha256() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha256,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 1);

        let index = Index::parse(
            &fs::read(repository_index_path(&git_dir)).expect("test operation should succeed"),
            ObjectFormat::Sha256,
        )
        .expect("test operation should succeed");
        assert_eq!(index.entries[0].path, b"hello.txt");
        assert_eq!(index.entries[0].oid.format(), ObjectFormat::Sha256);

        let tree_oid = write_tree_from_index(&git_dir, ObjectFormat::Sha256)
            .expect("test operation should succeed");
        assert_eq!(tree_oid.format(), ObjectFormat::Sha256);
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);
        let tree = odb
            .read_object(&tree_oid)
            .expect("test operation should succeed");
        assert_eq!(tree.object_type, ObjectType::Tree);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn write_tree_from_index_writes_nested_tree_objects() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("src")).expect("test operation should succeed");
        fs::write(root.join("README.md"), b"readme\n").expect("test operation should succeed");
        fs::write(root.join("src").join("lib.rs"), b"pub fn demo() {}\n")
            .expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("README.md"), PathBuf::from("src/lib.rs")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 2);
        let tree_oid = write_tree_from_index(&git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = odb
            .read_object(&tree_oid)
            .expect("test operation should succeed");
        assert_eq!(tree.object_type, ObjectType::Tree);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_reports_added_and_untracked_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        fs::write(root.join("extra.txt"), b"extra\n").expect("test operation should succeed");
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(
            status
                .iter()
                .map(ShortStatusEntry::line)
                .collect::<Vec<_>>(),
            vec!["A  hello.txt", "?? extra.txt"]
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-worktree-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test operation should succeed");
        path
    }

    fn index_entry_for<'a>(index: &'a Index, path: &[u8]) -> &'a IndexEntry {
        index
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("missing index entry for {}", String::from_utf8_lossy(path)))
    }

    fn read_index(git_dir: &Path) -> Index {
        Index::parse(
            &fs::read(repository_index_path(git_dir)).expect("test operation should succeed"),
            ObjectFormat::Sha1,
        )
        .expect("test operation should succeed")
    }

    /// Stages `paths` from the worktree, writes their tree, wraps it in a commit
    /// object, and points `refs/heads/main` + `HEAD` at it. Returns the commit
    /// id. After this call the index reflects the committed tree.
    fn build_commit(root: &Path, git_dir: &Path, paths: &[&str]) -> ObjectId {
        let path_bufs = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        add_paths_to_index(root, git_dir, ObjectFormat::Sha1, &path_bufs)
            .expect("test operation should succeed");
        let tree = write_tree_from_index(git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut body = Vec::new();
        body.extend_from_slice(format!("tree {tree}\n").as_bytes());
        body.extend_from_slice(b"author Test <test@example.com> 0 +0000\n");
        body.extend_from_slice(b"committer Test <test@example.com> 0 +0000\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"sparse fixture\n");
        let mut odb = FileObjectDatabase::from_git_dir(git_dir, ObjectFormat::Sha1);
        let commit = odb
            .write_object(EncodedObject::new(ObjectType::Commit, body))
            .expect("test operation should succeed");
        let refs = FileRefStore::new(git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(commit),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/main".into()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        commit
    }

    fn full_sparse(patterns: &[&[u8]]) -> SparseCheckout {
        SparseCheckout {
            patterns: patterns.iter().map(|pattern| pattern.to_vec()).collect(),
            sparse_index: false,
        }
    }

    #[test]
    fn apply_sparse_checkout_full_mode_skips_out_of_cone_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("in")).expect("test operation should succeed");
        fs::create_dir_all(root.join("out")).expect("test operation should succeed");
        fs::write(root.join("in").join("keep.txt"), b"keep\n")
            .expect("test operation should succeed");
        fs::write(root.join("out").join("drop.txt"), b"drop\n")
            .expect("test operation should succeed");
        fs::write(root.join("top.txt"), b"top\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["in/keep.txt", "out/drop.txt", "top.txt"]);

        // Full (non-cone) pattern: keep only the `in/` subtree.
        let sparse = full_sparse(&[b"/in/"]);
        let result = apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");

        assert!(root.join("in").join("keep.txt").exists());
        assert!(!root.join("out").join("drop.txt").exists());
        assert!(!root.join("top.txt").exists());
        assert!(result.materialized.contains(&b"in/keep.txt".to_vec()));
        assert!(result.skipped.contains(&b"out/drop.txt".to_vec()));
        assert!(result.skipped.contains(&b"top.txt".to_vec()));

        let index = read_index(&git_dir);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"in/keep.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"out/drop.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index, b"top.txt"
        )));
        // Out-of-cone entries are preserved in the index, just not on disk.
        assert_eq!(index.entries.len(), 3);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_toggle_rematerializes() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("a")).expect("test operation should succeed");
        fs::create_dir_all(root.join("b")).expect("test operation should succeed");
        fs::write(root.join("a").join("file.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("b").join("file.txt"), b"b\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["a/file.txt", "b/file.txt"]);

        // First narrow to `a/`.
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &full_sparse(&[b"/a/"]),
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(root.join("a").join("file.txt").exists());
        assert!(!root.join("b").join("file.txt").exists());
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"b/file.txt"
        )));

        // Now switch the cone to `b/`: `a/` must leave, `b/` must come back with
        // the correct content, and the skip-worktree bits must flip.
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &full_sparse(&[b"/b/"]),
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("a").join("file.txt").exists());
        assert!(root.join("b").join("file.txt").exists());
        assert_eq!(
            fs::read(root.join("b").join("file.txt")).expect("test operation should succeed"),
            b"b\n"
        );
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"a/file.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"b/file.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_cone_mode_matches_directory_prefixes() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("kept").join("nested"))
            .expect("test operation should succeed");
        fs::create_dir_all(root.join("other")).expect("test operation should succeed");
        fs::write(root.join("kept").join("a.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("kept").join("nested").join("b.txt"), b"b\n")
            .expect("test operation should succeed");
        fs::write(root.join("other").join("c.txt"), b"c\n").expect("test operation should succeed");
        fs::write(root.join("root.txt"), b"r\n").expect("test operation should succeed");
        build_commit(
            &root,
            &git_dir,
            &["kept/a.txt", "kept/nested/b.txt", "other/c.txt", "root.txt"],
        );

        // Standard cone patterns: top-level files plus the whole `kept/` tree.
        let sparse = SparseCheckout {
            patterns: vec![b"/*".to_vec(), b"!/*/".to_vec(), b"/kept/".to_vec()],
            sparse_index: false,
        };
        // Auto mode should detect cone shape on its own.
        assert!(patterns_are_cone(&sparse.patterns));
        apply_sparse_checkout(&root, &git_dir, ObjectFormat::Sha1, &sparse)
            .expect("test operation should succeed");

        assert!(root.join("root.txt").exists());
        assert!(root.join("kept").join("a.txt").exists());
        assert!(root.join("kept").join("nested").join("b.txt").exists());
        assert!(!root.join("other").join("c.txt").exists());

        let index = read_index(&git_dir);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"root.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"kept/a.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"kept/nested/b.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"other/c.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_honors_preexisting_skip_worktree_via_idempotence() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("in")).expect("test operation should succeed");
        fs::create_dir_all(root.join("out")).expect("test operation should succeed");
        fs::write(root.join("in").join("keep.txt"), b"keep\n")
            .expect("test operation should succeed");
        fs::write(root.join("out").join("drop.txt"), b"drop\n")
            .expect("test operation should succeed");
        build_commit(&root, &git_dir, &["in/keep.txt", "out/drop.txt"]);

        let sparse = full_sparse(&[b"/in/"]);
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("out").join("drop.txt").exists());

        // Re-applying the same spec is a no-op: the already-skipped file stays
        // absent and the bit stays set (we do not resurrect it).
        let result = apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("out").join("drop.txt").exists());
        assert!(root.join("in").join("keep.txt").exists());
        assert!(result.skipped.contains(&b"out/drop.txt".to_vec()));
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"out/drop.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn checkout_detached_sparse_only_writes_in_cone_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("keep")).expect("test operation should succeed");
        fs::create_dir_all(root.join("skip")).expect("test operation should succeed");
        fs::write(root.join("keep").join("a.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("skip").join("b.txt"), b"b\n").expect("test operation should succeed");
        let commit = build_commit(&root, &git_dir, &["keep/a.txt", "skip/b.txt"]);

        // The worktree is clean and matches the commit. A sparse checkout must
        // keep the in-cone file and evict the out-of-cone one.
        let sparse = full_sparse(&[b"/keep/"]);
        let result = checkout_detached_sparse(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &commit,
            b"Test <test@example.com> 0 +0000".to_vec(),
            b"checkout".to_vec(),
            &sparse,
        )
        .expect("test operation should succeed");
        assert_eq!(result.files, 2);

        assert!(root.join("keep").join("a.txt").exists());
        assert_eq!(
            fs::read(root.join("keep").join("a.txt")).expect("test operation should succeed"),
            b"a\n"
        );
        assert!(!root.join("skip").join("b.txt").exists());

        let index = read_index(&git_dir);
        assert_eq!(index.entries.len(), 2);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"keep/a.txt"
        )));
        let skipped = index_entry_for(&index, b"skip/b.txt");
        assert!(index_entry_skip_worktree(skipped));
        // The skipped entry still carries the committed blob id and mode.
        assert_eq!(skipped.mode, 0o100644);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    // ----- content filtering: EOL / autocrlf + clean/smudge drivers -----

    /// Build a [`GitConfig`] from raw config text.
    fn config_from(text: &str) -> GitConfig {
        GitConfig::parse(text.as_bytes()).expect("test operation should succeed")
    }

    /// Resolve attribute checks against an on-disk `.gitattributes` in `root`.
    fn attrs(root: &Path, path: &[u8]) -> Vec<AttributeCheck> {
        filter_attribute_checks(root, path).expect("test operation should succeed")
    }

    #[test]
    fn crlf_to_lf_collapses_only_pairs() {
        assert_eq!(convert_crlf_to_lf(b"a\r\nb\r\n"), b"a\nb\n");
        // A lone CR (no following LF) is preserved.
        assert_eq!(convert_crlf_to_lf(b"a\rb"), b"a\rb");
        // An already-LF stream is unchanged.
        assert_eq!(convert_crlf_to_lf(b"a\nb\n"), b"a\nb\n");
    }

    #[test]
    fn lf_to_crlf_does_not_double_convert() {
        assert_eq!(convert_lf_to_crlf(b"a\nb\n"), b"a\r\nb\r\n");
        // Existing CRLF is left intact (no extra CR added).
        assert_eq!(convert_lf_to_crlf(b"a\r\nb\r\n"), b"a\r\nb\r\n");
    }

    #[test]
    fn autocrlf_round_trip_clean_then_smudge() {
        // autocrlf=true: worktree CRLF -> blob LF on clean, blob LF -> worktree
        // CRLF on smudge.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let worktree = b"line1\r\nline2\r\n";
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"file.txt", worktree)
            .expect("test operation should succeed");
        assert_eq!(blob, b"line1\nline2\n", "clean must normalize CRLF to LF");
        let restored = apply_smudge_filter_with_attributes(&config, &checks, b"file.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            restored, worktree,
            "smudge must restore CRLF from the LF blob"
        );
    }

    #[test]
    fn autocrlf_input_normalizes_on_clean_but_not_smudge() {
        // autocrlf=input: clean normalizes to LF, smudge leaves LF as-is.
        let config = config_from("[core]\n\tautocrlf = input\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"file.txt", b"a\r\nb\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"a\nb\n");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"file.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            smudged, b"a\nb\n",
            "input mode must not add carriage returns"
        );
    }

    #[test]
    fn eol_crlf_attribute_drives_conversion_without_config() {
        // No core.autocrlf; the `eol=crlf` attribute alone forces conversion.
        let config = config_from("");
        let checks = vec![AttributeCheck {
            attribute: b"eol".to_vec(),
            state: Some(AttributeState::Value(b"crlf".to_vec())),
        }];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"a.txt", b"x\r\ny\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"x\ny\n");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"a.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(smudged, b"x\r\ny\r\n");
    }

    #[test]
    fn binary_attribute_disables_eol_conversion() {
        // `-text` (binary) must leave CRLF/NUL content untouched in both
        // directions even when autocrlf=true.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"text".to_vec(),
            state: Some(AttributeState::Unset),
        }];
        let content = b"\x00\x01\r\n\x02\r\n".to_vec();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"data.bin", &content)
            .expect("test operation should succeed");
        assert_eq!(blob, content, "binary file must not be CRLF-normalized");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"data.bin", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            smudged, content,
            "binary file must not gain carriage returns"
        );
    }

    #[test]
    fn autocrlf_auto_skips_binary_looking_content() {
        // text=auto (via autocrlf) must not convert content that contains NUL.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let content = b"a\r\n\x00b\r\n".to_vec();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f", &content)
            .expect("test operation should succeed");
        assert_eq!(blob, content, "binary-looking content stays untouched");
    }

    #[test]
    fn autocrlf_via_add_and_checkout_round_trips() {
        // End-to-end: a CRLF worktree file is stored as an LF blob by the
        // filtered add path, and restored as CRLF by the filtered checkout.
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let config = config_from("[core]\n\tautocrlf = true\n");

        fs::write(root.join("crlf.txt"), b"alpha\r\nbeta\r\n")
            .expect("test operation should succeed");
        add_paths_to_index_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("crlf.txt")],
            &config,
        )
        .expect("test operation should succeed");

        // The stored blob must be LF-normalized.
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"crlf.txt");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let blob = odb
            .read_object(&entry.oid)
            .expect("test operation should succeed");
        assert_eq!(blob.body, b"alpha\nbeta\n");

        // Commit and point HEAD at it, then re-checkout with smudge filtering.
        let tree = write_tree_from_index(&git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut body = Vec::new();
        body.extend_from_slice(format!("tree {tree}\n").as_bytes());
        body.extend_from_slice(b"author T <t@e> 0 +0000\ncommitter T <t@e> 0 +0000\n\nm\n");
        let mut odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let commit = odb
            .write_object(EncodedObject::new(ObjectType::Commit, body))
            .expect("test operation should succeed");
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Direct(commit),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        // Make the worktree match the committed (LF) blob so the tree is clean
        // for checkout; `short_status`/`worktree_entries` compare by content
        // hash and are not filter-aware. Checkout will then smudge it to CRLF.
        fs::write(root.join("crlf.txt"), b"alpha\nbeta\n").expect("test operation should succeed");
        checkout_detached_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &commit,
            b"T <t@e> 0 +0000".to_vec(),
            b"co".to_vec(),
            &config,
        )
        .expect("test operation should succeed");
        assert_eq!(
            fs::read(root.join("crlf.txt")).expect("test operation should succeed"),
            b"alpha\r\nbeta\r\n",
            "checkout must restore CRLF line endings"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn driver_filter_clean_and_smudge_transform_both_directions() {
        // filter=case: clean upper-cases (worktree -> blob), smudge lower-cases
        // (blob -> worktree).
        let config =
            config_from("[filter \"case\"]\n\tclean = tr a-z A-Z\n\tsmudge = tr A-Z a-z\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"case".to_vec())),
        }];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f.txt", b"Hello World")
            .expect("test operation should succeed");
        assert_eq!(blob, b"HELLO WORLD", "clean driver must upper-case");
        let worktree =
            apply_smudge_filter_with_attributes(&config, &checks, b"f.txt", b"HELLO WORLD")
                .expect("test operation should succeed");
        assert_eq!(worktree, b"hello world", "smudge driver must lower-case");
    }

    #[test]
    fn driver_filter_resolved_from_gitattributes_file() {
        // The filter name is read from a real `.gitattributes`, the commands from
        // config; exercises the public worktree-rooted entry points.
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join(".gitattributes"), b"*.dat filter=rot\n")
            .expect("test operation should succeed");
        let config =
            config_from("[filter \"rot\"]\n\tclean = sed s/a/b/g\n\tsmudge = sed s/b/a/g\n");
        // Clean reads attributes from the live worktree `.gitattributes`.
        let blob = apply_clean_filter(&root, &git_dir, &config, b"x.dat", b"banana")
            .expect("test operation should succeed");
        assert_eq!(blob, b"bbnbnb");
        // Smudge reads attributes from the index (the worktree file may not
        // exist yet during checkout), so stage `.gitattributes` first.
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from(".gitattributes")],
        )
        .expect("test operation should succeed");
        let smudged = apply_smudge_filter(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &config,
            b"x.dat",
            &blob,
        )
        .expect("test operation should succeed");
        // sed s/b/a/g is not a perfect inverse, but verifies the smudge command
        // ran on the blob bytes.
        assert_eq!(smudged, b"aanana");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn required_filter_failure_is_fatal() {
        // A required filter whose command fails must surface an error.
        let config = config_from("[filter \"boom\"]\n\tclean = false\n\trequired = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"boom".to_vec())),
        }];
        let err = apply_clean_filter_with_attributes(&config, &checks, b"f", b"data")
            .expect_err("required filter failure must error");
        assert!(matches!(err, GitError::Command(_)), "got {err:?}");
    }

    #[test]
    fn required_filter_missing_command_is_fatal() {
        // required=true but no clean command for this direction is also fatal.
        let config = config_from("[filter \"need\"]\n\tsmudge = cat\n\trequired = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"need".to_vec())),
        }];
        let err = apply_clean_filter_with_attributes(&config, &checks, b"f", b"data")
            .expect_err("required filter without a clean command must error");
        assert!(matches!(err, GitError::Command(_)), "got {err:?}");
    }

    #[test]
    fn non_required_filter_failure_passes_through() {
        // A non-required filter that fails must pass the content through
        // unchanged rather than erroring.
        let config = config_from("[filter \"opt\"]\n\tclean = false\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"opt".to_vec())),
        }];
        let out = apply_clean_filter_with_attributes(&config, &checks, b"f", b"keepme")
            .expect("test operation should succeed");
        assert_eq!(
            out, b"keepme",
            "optional filter failure passes content through"
        );
    }

    #[test]
    fn filter_with_no_command_is_noop() {
        // filter=name with no configured commands and not required is ignored.
        let config = config_from("");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"ghost".to_vec())),
        }];
        let out = apply_clean_filter_with_attributes(&config, &checks, b"f", b"unchanged")
            .expect("test operation should succeed");
        assert_eq!(out, b"unchanged");
    }

    #[test]
    fn driver_and_eol_compose_on_clean_and_smudge() {
        // filter=case + autocrlf=true: clean runs the driver then CRLF->LF;
        // smudge runs LF->CRLF then the driver.
        let config = config_from(
            "[core]\n\tautocrlf = true\n[filter \"case\"]\n\tclean = tr a-z A-Z\n\tsmudge = tr A-Z a-z\n",
        );
        let checks = vec![
            AttributeCheck {
                attribute: b"filter".to_vec(),
                state: Some(AttributeState::Value(b"case".to_vec())),
            },
            AttributeCheck {
                attribute: b"text".to_vec(),
                state: Some(AttributeState::Set),
            },
        ];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f.txt", b"ab\r\ncd\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"AB\nCD\n", "clean: upper-case then CRLF->LF");
        let worktree = apply_smudge_filter_with_attributes(&config, &checks, b"f.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            worktree, b"ab\r\ncd\r\n",
            "smudge: LF->CRLF then lower-case"
        );
    }

    #[test]
    fn attrs_helper_reads_filter_from_disk() {
        let root = temp_root();
        fs::write(root.join(".gitattributes"), b"*.txt text\n*.bin -text\n")
            .expect("test operation should succeed");
        let text = attrs(&root, b"a.txt");
        assert!(
            text.iter()
                .any(|c| c.attribute == b"text" && c.state == Some(AttributeState::Set))
        );
        let bin = attrs(&root, b"a.bin");
        assert!(
            bin.iter()
                .any(|c| c.attribute == b"text" && c.state == Some(AttributeState::Unset))
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    /// Builds a stat cache holding a single stage-0 entry whose size+mtime match
    /// `file`'s real metadata, with the index-file mtime placed strictly after
    /// the entry mtime so the entry reads as non-racy by default. The entry's oid
    /// is `oid` and its mode is `mode`.
    fn stat_cache_for(file: &Path, oid: ObjectId, mode: u32) -> (IndexStatCache, IndexEntry) {
        let metadata = fs::metadata(file).expect("test operation should succeed");
        let mut entry = index_entry_from_metadata(b"f.txt".to_vec(), oid, &metadata);
        entry.mode = mode;
        let index_mtime = Some((u64::from(entry.mtime_seconds) + 10, 0));
        let mut entries = BTreeMap::new();
        entries.insert(entry.path.as_bytes().to_vec(), entry.clone());
        (
            IndexStatCache {
                entries,
                index_mtime,
            },
            entry,
        )
    }

    #[test]
    fn reuse_tracked_entry_only_reuses_clean_non_racy_match() {
        let root = temp_root();
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        let file = root.join("f.txt");
        let metadata = fs::metadata(&file).expect("test operation should succeed");
        let real_mode = file_mode(&metadata);
        let oid = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec())
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        // Clean, non-racy, matching stat + mode -> reuse the cached oid.
        let (cache, _) = stat_cache_for(&file, oid, real_mode);
        let reused = cache.reuse_tracked_entry(b"f.txt", &metadata);
        assert_eq!(
            reused,
            Some(TrackedEntry {
                mode: real_mode,
                oid,
            }),
            "a clean non-racy stat+mode match must reuse the staged oid"
        );

        // No stage-0 entry for the path -> must hash.
        assert_eq!(
            cache.reuse_tracked_entry(b"other.txt", &metadata),
            None,
            "a path with no cached entry must fall through to hashing"
        );

        // Size differs from the file -> must hash.
        let (mut size_cache, mut shrunk) = stat_cache_for(&file, oid, real_mode);
        shrunk.size = shrunk.size.saturating_sub(1);
        size_cache.entries.insert(shrunk.path.to_vec(), shrunk);
        assert_eq!(
            size_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a size mismatch must fall through to hashing"
        );

        // Mode differs (e.g. a chmod that did not move mtime) -> must hash.
        let (mode_cache, _) = stat_cache_for(&file, oid, 0o100755);
        assert_eq!(
            mode_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a mode mismatch must fall through to hashing"
        );

        // Racily clean (index mtime not strictly after the entry mtime) -> hash.
        let (mut racy_cache, entry) = stat_cache_for(&file, oid, real_mode);
        racy_cache.index_mtime = Some((
            u64::from(entry.mtime_seconds),
            u64::from(entry.mtime_nanoseconds),
        ));
        assert_eq!(
            racy_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a racily-clean entry must always be re-hashed"
        );

        // Unknown index mtime is treated as racy -> hash.
        let (mut unknown_cache, _) = stat_cache_for(
            &file,
            EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec())
                .object_id(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
            real_mode,
        );
        unknown_cache.index_mtime = None;
        assert_eq!(
            unknown_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "an unknown index mtime must be treated conservatively as racy"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_detects_same_length_content_change() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"aaaa\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        // Overwrite with the SAME byte length but different content. Right after
        // staging the entry is racily clean (index mtime >= entry mtime), so the
        // stat shortcut must not be trusted and the change must surface as M.
        fs::write(root.join("f.txt"), b"bbbb\n").expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(
            status
                .iter()
                .map(ShortStatusEntry::line)
                .collect::<Vec<_>>(),
            vec![" M f.txt"],
            "a same-length content change must be reported modified"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_clean_after_byte_identical_rewrite() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        // Rewrite with byte-identical content; the mtime moves so the stat
        // shortcut declines to reuse and the fallback hash proves it clean.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            status.is_empty(),
            "a byte-identical rewrite must be clean via the fallback hash, got {status:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_trusts_stat_cache_and_skips_rehash() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);

        // Plant a BOGUS oid in the stage-0 entry while preserving its size+mtime,
        // so a real re-hash of the (unchanged) worktree file would NOT match it.
        let index_path = repository_index_path(&git_dir);
        let mut index = read_index(&git_dir);
        let bogus = ObjectId::from_hex(ObjectFormat::Sha1, &"0".repeat(40))
            .expect("test operation should succeed");
        let real_oid = index_entry_for(&index, b"f.txt").oid;
        assert_ne!(
            real_oid, bogus,
            "fixture oid should differ from the bogus oid"
        );
        index
            .entries
            .iter_mut()
            .find(|entry| entry.path == b"f.txt")
            .expect("test operation should succeed")
            .oid = bogus.clone();
        fs::write(
            &index_path,
            index
                .write(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        // Make the index file STRICTLY newer than the entry mtime (non-racy) by
        // waiting past one-second filesystem granularity and rewriting it, so the
        // racy-clean guard does not force a re-hash.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(
            &index_path,
            fs::read(&index_path).expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        // The file is unchanged on disk, so a trusted stat reuses the bogus index
        // oid for the worktree entry: worktree-oid == index-oid == bogus, so the
        // WORKTREE column is clean. Had status re-hashed the file, the real oid
        // would differ from the bogus index oid and the worktree column would be
        // 'M'. (The index-vs-HEAD column is 'M' because we corrupted the index
        // oid away from HEAD; that is expected and not what this test asserts.)
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let entry = status
            .iter()
            .find(|entry| entry.path == b"f.txt")
            .expect("f.txt should appear (its index oid now differs from HEAD)");
        assert_eq!(
            entry.worktree, b' ',
            "non-racy stat match must trust the cached oid (no re-hash); worktree column was {}",
            entry.worktree as char
        );
        assert_eq!(
            entry.index_oid.as_ref(),
            Some(&bogus),
            "the worktree entry must have reused the planted bogus index oid, not the real hash"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_empty_on_unborn_repository() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            status.is_empty(),
            "an unborn repository with an empty worktree must be clean, got {status:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn untracked_paths_skips_embedded_git_internals() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let nested = root.join("not-a-submodule");
        fs::create_dir_all(nested.join(".git")).expect("test operation should succeed");
        fs::write(nested.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        fs::write(nested.join("file.txt"), b"inside\n").expect("test operation should succeed");
        let paths = untracked_paths(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            paths.iter().any(|path| path == b"not-a-submodule/"),
            "embedded repository directory should be listed, got {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with(b"not-a-submodule/.git")),
            "embedded .git internals must not be listed, got {paths:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn untracked_paths_lists_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        fs::write(root.join("target.txt"), b"target\n").expect("test operation should succeed");
        symlink(root.join("target.txt"), root.join("path1")).expect("create symlink");
        let paths = untracked_paths(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            paths.contains(&b"path1".to_vec()),
            "untracked symlink must be listed, got {paths:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }
}
