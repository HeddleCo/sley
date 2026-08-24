//! Entry filtering and ordering: pathspec/max-depth filters, `--order-file`,
//! rename reversal, `-I<regex>` compilation, submodule-ignore policy, and the
//! tree-to-tree patch used by sequencer patch files.

use super::content::repo_path_to_path;
use super::options::{
    DiffRenderOptions, LazyObjectFetch, SubmoduleDiffFormat, SubmoduleIgnoreMode,
    parse_submodule_ignore_mode,
};
use super::patch_entry::write_diff_patch_entry;
use crate::{IndexGitlinkEntry, NameStatus, NameStatusEntry};
use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_grep::Regex;
use sley_odb::FileObjectDatabase;
use sley_pathspec::{LsFilesPathFilter, pathspec_filters_match};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Upstream `DIRTY_SUBMODULE_MODIFIED`: staged/unstaged changes to tracked
/// files inside a checked-out submodule.
pub const DIRTY_SUBMODULE_MODIFIED: u8 = 1;
/// Upstream `DIRTY_SUBMODULE_UNTRACKED`: untracked content inside a
/// checked-out submodule.
pub const DIRTY_SUBMODULE_UNTRACKED: u8 = 2;

/// The resolved submodule-ignore configuration for a diff invocation.
/// Precedence per path (upstream run_diff_files / set_diffopt_flags_from_
/// submodule_config): an explicit `--ignore-submodules` wins over everything;
/// otherwise `submodule.<name>.ignore` in the repo config, then in
/// `.gitmodules`; otherwise `diff.ignoreSubmodules`; otherwise the implicit
/// untracked-ignoring default.
pub struct SubmoduleDiffConfig {
    cli: Option<SubmoduleIgnoreMode>,
    base: SubmoduleIgnoreMode,
    per_path: HashMap<Vec<u8>, SubmoduleIgnoreMode>,
}

impl SubmoduleDiffConfig {
    fn effective(&self, path: &[u8]) -> SubmoduleIgnoreMode {
        if let Some(mode) = self.cli {
            return mode;
        }
        if let Some(mode) = self.per_path.get(path) {
            return *mode;
        }
        self.base
    }
}

/// Reads the effective repository config (includes + command-line overrides
/// applied); `None` when unreadable. Hosts without config access pass a
/// closure returning `None`, which disables only the config-derived ignore
/// layers.
pub type LoadRepoConfig<'a> = &'a (dyn Fn(&Path) -> Option<GitConfig> + 'a);

pub fn submodule_diff_config_with_config(
    git_dir: &Path,
    worktree_root: Option<&Path>,
    cli: Option<SubmoduleIgnoreMode>,
    repo_config: Option<&GitConfig>,
    load_repo_config: LoadRepoConfig<'_>,
) -> SubmoduleDiffConfig {
    let loaded_config = repo_config
        .is_none()
        .then(|| load_repo_config(git_dir))
        .flatten();
    let repo_config = repo_config.or(loaded_config.as_ref());
    let base = repo_config
        .and_then(|config| config.get("diff", None, "ignoresubmodules"))
        .and_then(parse_submodule_ignore_mode)
        .unwrap_or(SubmoduleIgnoreMode::Untracked);
    let mut per_path = HashMap::new();
    if let Some(root) = worktree_root
        && let Ok(gitmodules) = GitConfig::read(root.join(".gitmodules"))
    {
        for section in &gitmodules.sections {
            if section.name != "submodule" {
                continue;
            }
            let Some(name) = section.subsection.as_deref() else {
                continue;
            };
            let value_of = |key: &str| {
                section
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| entry.key.eq_ignore_ascii_case(key))
                    .and_then(|entry| entry.value.as_deref())
            };
            let Some(path) = value_of("path") else {
                continue;
            };
            let ignore = repo_config
                .and_then(|config| config.get("submodule", Some(name), "ignore"))
                .or_else(|| value_of("ignore"));
            if let Some(mode) = ignore.and_then(parse_submodule_ignore_mode) {
                per_path.insert(path.as_bytes().to_vec(), mode);
            }
        }
    }
    SubmoduleDiffConfig {
        cli,
        base,
        per_path,
    }
}

/// Drop gitlink entries whose effective ignore mode is `all`.
pub fn apply_submodule_ignore_filter(
    entries: Vec<NameStatusEntry>,
    config: &SubmoduleDiffConfig,
) -> Vec<NameStatusEntry> {
    entries
        .into_iter()
        .filter(|entry| {
            let gitlink = entry.old_mode == Some(0o160000) || entry.new_mode == Some(0o160000);
            !(gitlink && config.effective(&entry.path) == SubmoduleIgnoreMode::All)
        })
        .collect()
}

/// Supplies the worktree-bound pieces of dirty-submodule detection: index
/// gitlink enumeration and per-submodule dirt bitmasks.
pub trait SubmoduleDirtSource {
    /// Staged gitlinks from `git_dir`'s index; `Ok(None)` when no index exists.
    ///
    /// # Errors
    /// Propagates index read errors.
    fn index_gitlinks(
        &self,
        git_dir: &Path,
        format: ObjectFormat,
    ) -> Result<Option<Vec<IndexGitlinkEntry>>>;

    /// The upstream `submodule_dirt()` bitmask (`DIRTY_SUBMODULE_*`) for the
    /// checked-out submodule at `sub_root`.
    fn submodule_dirt(&self, sub_root: &Path) -> u8;
}

/// For a worktree-involved diff: find every staged gitlink whose submodule
/// tree is dirty under its effective ignore mode (the `-dirty` suffix set),
/// and append a Modified pair for dirty submodules whose checked-out commit
/// still matches the staged oid (which the map comparison alone would skip).
#[allow(clippy::too_many_arguments)]
pub fn collect_dirty_submodules(
    entries: &mut Vec<NameStatusEntry>,
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    config: &SubmoduleDiffConfig,
    precomputed_gitlinks: Option<&[IndexGitlinkEntry]>,
    source: &dyn SubmoduleDirtSource,
) -> Result<HashMap<Vec<u8>, u8>> {
    let owned_gitlinks;
    let gitlinks: &[IndexGitlinkEntry] = match precomputed_gitlinks {
        Some(gitlinks) => gitlinks,
        None => {
            owned_gitlinks = source.index_gitlinks(git_dir, format)?.unwrap_or_default();
            &owned_gitlinks
        }
    };
    collect_dirty_submodules_from_gitlinks(entries, worktree_root, config, gitlinks, source)
}

fn collect_dirty_submodules_from_gitlinks(
    entries: &mut Vec<NameStatusEntry>,
    worktree_root: &Path,
    config: &SubmoduleDiffConfig,
    gitlinks: &[IndexGitlinkEntry],
    source: &dyn SubmoduleDirtSource,
) -> Result<HashMap<Vec<u8>, u8>> {
    let mut dirty = HashMap::new();
    let mut injected = false;
    for entry in gitlinks {
        let path = entry.path.as_bytes();
        let mode = config.effective(path);
        if matches!(mode, SubmoduleIgnoreMode::All | SubmoduleIgnoreMode::Dirty) {
            continue;
        }
        let sub_root = worktree_root.join(repo_path_to_path(path));
        let dirt = source.submodule_dirt(&sub_root);
        let counts = dirt & DIRTY_SUBMODULE_MODIFIED != 0
            || (mode == SubmoduleIgnoreMode::None && dirt & DIRTY_SUBMODULE_UNTRACKED != 0);
        if !counts {
            continue;
        }
        let visible_dirt = match mode {
            SubmoduleIgnoreMode::None => dirt,
            SubmoduleIgnoreMode::Untracked => dirt & !DIRTY_SUBMODULE_UNTRACKED,
            SubmoduleIgnoreMode::Dirty | SubmoduleIgnoreMode::All => 0,
        };
        if visible_dirt == 0 {
            continue;
        }
        dirty.insert(path.to_vec(), visible_dirt);
        if !entries.iter().any(|existing| existing.path[..] == *path) {
            entries.push(NameStatusEntry {
                status: NameStatus::Modified,
                path: path.to_vec().into(),
                old_path: None,
                old_mode: Some(0o160000),
                new_mode: Some(0o160000),
                old_oid: Some(entry.oid),
                new_oid: Some(entry.oid),
            });
            injected = true;
        }
    }
    if injected {
        entries.sort_by(|left, right| left.path.cmp(&right.path));
    }
    Ok(dirty)
}

/// Render the `git diff-tree -p`-equivalent patch between two trees, byte-for-byte
/// matching what `git show`/`log_tree_commit` writes. Used by the sequencer's
/// `make_patch` (`.git/rebase-merge/patch`) so the stopped-pick patch carries the
/// real `index <old>..<new> <mode>` line, collapsed single-line hunk headers
/// (`@@ -1 +1 @@`), and no spurious blank lines — exactly the renderer that drives
/// porcelain diff output, rather than a hand-rolled approximation.
///
/// Matches git `make_patch`: `DEFAULT_ABBREV` (7) index hashes, three lines of
/// context, no rename detection (`log_tree_commit` does not honor `diff.renames`),
/// `a/`/`b/` prefixes. `lazy_fetch` routes missing-object hydration through the
/// host's promisor support.
pub fn render_tree_to_tree_patch(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
    big_file_threshold: u64,
) -> Result<Vec<u8>> {
    let entries = crate::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        crate::DiffNameStatusOptions::default(),
    )?;
    // Batch-prefetch every blob the patch body will open (git's
    // `diff_queued_diff_prefetch` / `promisor_remote_get_direct`).
    if let Some(fetch) = lazy_fetch {
        fetch.prefetch_entry_blobs(db, &entries, false)?;
    }
    let mut out: Vec<u8> = Vec::new();
    for entry in &entries {
        write_diff_patch_entry(
            &mut out,
            entry,
            DiffRenderOptions {
                binary: false,
                anchors: &[],
                allow_textconv: false,
                db,
                lazy_fetch,
                worktree_root: None,
                use_worktree_new: false,
                format,
                abbrev: 7,
                src_prefix: "a/",
                dst_prefix: "b/",
                context: 3,
                userdiff: None,
                funcname: None,
                colors: None,
                word_diff: None,
                line_indicators: crate::render::LineIndicators::default(),
                suppress_blank_empty: false,
                no_index_contents: None,
                submodule_format: SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                color_moved: None,
                interhunk: 0,
                ws_ignore: crate::WsIgnore::default(),
                diff_algorithm: crate::DiffAlgorithm::Myers,
                ignore_blank_lines: false,
                ignore_regexes: &[],
                line_ranges: None,
                indent_heuristic: true,
                big_file_threshold,
                submodule_render: None,
            },
        )?;
    }
    Ok(out)
}

/// Compile the `-I<regex>` (`--ignore-matching-lines`) patterns into ERE
/// matchers. A malformed pattern fails like git's `diff_opt_ignore_regex`:
/// `error: invalid regex given to -I: '<pat>'` and exit code 129.
pub fn compile_ignore_matching_regexes(patterns: &[String]) -> Result<Vec<Regex>> {
    let mut compiled = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        match Regex::compile(pattern, sley_grep::RegexMode::Ere, false, false) {
            Ok(regex) => compiled.push(regex),
            Err(_) => {
                eprintln!("error: invalid regex given to -I: '{pattern}'");
                return Err(GitError::Exit(129));
            }
        }
    }
    Ok(compiled)
}

pub fn apply_diff_order_file(
    mut entries: Vec<NameStatusEntry>,
    order_file: Option<&str>,
) -> Result<Vec<NameStatusEntry>> {
    let Some(order_file) = order_file else {
        return Ok(entries);
    };
    if order_file == "/dev/null" {
        return Ok(entries);
    }
    let path = Path::new(order_file);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let contents = fs::read(&path)?;
    let patterns: Vec<Vec<u8>> = contents
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(|line| line.to_vec())
        .collect();
    if patterns.is_empty() {
        return Ok(entries);
    }
    entries.sort_by_key(|entry| diff_order_rank(entry, &patterns).unwrap_or(usize::MAX));
    Ok(entries)
}

fn diff_order_rank(entry: &NameStatusEntry, patterns: &[Vec<u8>]) -> Option<usize> {
    patterns.iter().position(|pattern| {
        diff_order_pattern_matches(pattern, entry.path.as_bytes())
            || entry
                .old_path
                .as_ref()
                .is_some_and(|old| diff_order_pattern_matches(pattern, old.as_bytes()))
    })
}

fn diff_order_pattern_matches(pattern: &[u8], path: &[u8]) -> bool {
    pattern == path || diff_order_glob_matches(pattern, path)
}

fn diff_order_glob_matches(pattern: &[u8], text: &[u8]) -> bool {
    let mut p = 0usize;
    let mut t = 0usize;
    let mut star: Option<usize> = None;
    let mut match_after_star = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            match_after_star = t;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            match_after_star += 1;
            t = match_after_star;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

pub fn apply_diff_pathspec(
    entries: Vec<NameStatusEntry>,
    pathspec: &DiffPathspec,
) -> Vec<NameStatusEntry> {
    if pathspec.is_empty() {
        return entries;
    }
    let mut filtered = Vec::new();
    for entry in entries {
        if let Some(old_path) = &entry.old_path {
            let old_matches = pathspec.matches(old_path);
            let new_matches = pathspec.matches(&entry.path);
            if matches!(entry.status, NameStatus::Copied(_)) {
                match (old_matches, new_matches) {
                    (true, true) => filtered.push(entry),
                    (false, true) => filtered.push(NameStatusEntry {
                        status: NameStatus::Added,
                        path: entry.path,
                        old_path: None,
                        old_mode: None,
                        new_mode: entry.new_mode,
                        old_oid: None,
                        new_oid: entry.new_oid,
                    }),
                    (true, false) | (false, false) => {}
                }
            } else {
                match (old_matches, new_matches) {
                    (true, true) => filtered.push(entry),
                    (true, false) => filtered.push(NameStatusEntry {
                        status: NameStatus::Deleted,
                        path: old_path.clone(),
                        old_path: None,
                        old_mode: entry.old_mode,
                        new_mode: None,
                        old_oid: entry.old_oid,
                        new_oid: None,
                    }),
                    (false, true) => filtered.push(NameStatusEntry {
                        status: NameStatus::Added,
                        path: entry.path,
                        old_path: None,
                        old_mode: None,
                        new_mode: entry.new_mode,
                        old_oid: None,
                        new_oid: entry.new_oid,
                    }),
                    (false, false) => {}
                }
            }
        } else if pathspec.matches(&entry.path) {
            filtered.push(entry);
        }
    }
    filtered
}

pub fn apply_diff_max_depth(
    entries: Vec<NameStatusEntry>,
    pathspec: &DiffPathspec,
    max_depth: Option<i64>,
) -> Vec<NameStatusEntry> {
    let Some(max_depth) = max_depth else {
        return entries;
    };
    if max_depth < 0 {
        return entries;
    }
    entries
        .into_iter()
        .filter(|entry| pathspec.within_max_depth(&entry.path, max_depth))
        .collect()
}

pub fn parse_diff_max_depth(value: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("error: option `max-depth' expects a numerical value");
        GitError::Exit(129)
    })
}

pub fn reverse_diff_entries(entries: Vec<NameStatusEntry>) -> Vec<NameStatusEntry> {
    let mut reversed = entries
        .into_iter()
        .map(reverse_diff_entry)
        .collect::<Vec<_>>();
    reversed.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
            .then_with(|| left.status.code().cmp(&right.status.code()))
    });
    reversed
}

pub fn reverse_diff_entry(entry: NameStatusEntry) -> NameStatusEntry {
    match entry.status {
        NameStatus::Added => NameStatusEntry {
            status: NameStatus::Deleted,
            old_mode: entry.new_mode,
            new_mode: None,
            old_oid: entry.new_oid,
            new_oid: None,
            ..entry
        },
        NameStatus::Deleted => NameStatusEntry {
            status: NameStatus::Added,
            old_mode: None,
            new_mode: entry.old_mode,
            old_oid: None,
            new_oid: entry.old_oid,
            ..entry
        },
        // A reversed typechange is still a typechange (the `S_IFMT` bits still
        // differ once the two sides are swapped), so it keeps its status and just
        // flips the mode/oid pair like a modify.
        NameStatus::Modified | NameStatus::TypeChanged => NameStatusEntry {
            old_mode: entry.new_mode,
            new_mode: entry.old_mode,
            old_oid: entry.new_oid,
            new_oid: entry.old_oid,
            ..entry
        },
        NameStatus::Renamed(score) => {
            let new_path = entry
                .old_path
                .clone()
                .expect("rename entries include old_path");
            NameStatusEntry {
                status: NameStatus::Renamed(score),
                path: new_path,
                old_path: Some(entry.path),
                old_mode: entry.new_mode,
                new_mode: entry.old_mode,
                old_oid: entry.new_oid,
                new_oid: entry.old_oid,
            }
        }
        NameStatus::Copied(_) => NameStatusEntry {
            status: NameStatus::Deleted,
            old_path: None,
            old_mode: entry.new_mode,
            new_mode: None,
            old_oid: entry.new_oid,
            new_oid: None,
            ..entry
        },
        // An unmerged marker has no directional content to flip.
        NameStatus::Unmerged => entry,
    }
}

pub fn validate_diff_rename_limit(value: &str) -> Result<()> {
    let value = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(value);
    let value = value
        .strip_suffix('k')
        .or_else(|| value.strip_suffix('m'))
        .or_else(|| value.strip_suffix('g'))
        .unwrap_or(value);
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(diff_rename_limit_requires_integer_error())
    }
}

pub fn diff_rename_limit_requires_integer_error() -> GitError {
    eprintln!("error: switch `l' expects an integer value with an optional k/m/g suffix");
    GitError::Exit(129)
}

/// Pathspec matching over diff entries (`LsFilesPathFilter`-backed). Hosts
/// construct instances through their own pathspec normalization front ends.
#[derive(Default)]
pub struct DiffPathspec {
    filters: Vec<LsFilesPathFilter>,
}

impl DiffPathspec {
    /// Assemble from pre-parsed filters.
    pub fn from_filters(filters: Vec<LsFilesPathFilter>) -> Self {
        Self { filters }
    }

    /// Expose the parsed filters for hosts that need post-processing.
    pub fn filters(&self) -> &[LsFilesPathFilter] {
        &self.filters
    }

    pub fn matches(&self, path: &[u8]) -> bool {
        if self.filters.is_empty() {
            return true;
        }
        pathspec_filters_match(&self.filters, path)
    }

    pub fn within_max_depth(&self, path: &[u8], max_depth: i64) -> bool {
        self.relative_depth(path)
            .is_some_and(|depth| depth <= max_depth)
    }

    fn relative_depth(&self, path: &[u8]) -> Option<i64> {
        if self.filters.is_empty() {
            return Some(diff_path_slash_depth(path));
        }
        let mut saw_include = false;
        let mut best: Option<i64> = None;
        for filter in &self.filters {
            if filter.is_exclude() {
                continue;
            }
            saw_include = true;
            if let Some(depth) = diff_relative_depth_from_spec(filter.element.pattern(), path) {
                best = Some(best.map_or(depth, |current| current.min(depth)));
            }
        }
        if saw_include {
            best
        } else {
            Some(diff_path_slash_depth(path))
        }
    }

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }
}

fn diff_relative_depth_from_spec(spec: &[u8], path: &[u8]) -> Option<i64> {
    let spec = spec.strip_suffix(b"/").unwrap_or(spec);
    if spec.is_empty() || spec == b"." {
        return Some(diff_path_slash_depth(path));
    }
    if path == spec {
        return Some(0);
    }
    if path.len() > spec.len() && path.starts_with(spec) && path.get(spec.len()) == Some(&b'/') {
        return Some(diff_path_component_count(&path[spec.len() + 1..]));
    }
    None
}

fn diff_path_slash_depth(path: &[u8]) -> i64 {
    path.iter().filter(|byte| **byte == b'/').count() as i64
}

fn diff_path_component_count(path: &[u8]) -> i64 {
    if path.is_empty() {
        0
    } else {
        diff_path_slash_depth(path) + 1
    }
}
