//! `git ls-files` pathspec construction and path rendering helpers.

use std::cell::Cell;
use std::fs;
use std::path::Path;

use sley::{GitError, Result};
use sley_pathspec::{
    LsFilesPathFilter, parse_normalized_pathspec_element, pathspec_attrs_match_with,
    pathspec_filters_have_include, pathspec_filters_match_with,
};

use crate::session_globals::attribute_checks_for_matching;
use crate::sley_index;
use crate::sley_worktree;

use sley::plumbing::sley_core::paths::relative_path_bytes;

pub(crate) fn index_entry_stage(entry: &sley_index::IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

pub(crate) struct LsFilesPathspec {
    prefix: Vec<u8>,
    full_name: bool,
    pub(crate) filters: Vec<LsFilesPathFilter>,
    attributes: Option<sley_worktree::StandardAttributeMatcher>,
}

impl LsFilesPathspec {
    pub(crate) fn new(
        cwd: &Path,
        worktree_root: &Path,
        full_name: bool,
        path_args: &[String],
        magic: sley_worktree::PathspecMatchMagic,
    ) -> Result<Self> {
        let root = fs::canonicalize(worktree_root)?;
        let cwd = fs::canonicalize(cwd)?;
        let (relative, pathspec_cwd) = match cwd.strip_prefix(&root) {
            Ok(relative) => (relative, cwd.as_path()),
            Err(_) => (Path::new(""), root.as_path()),
        };
        let prefix = relative.to_string_lossy().replace('\\', "/").into_bytes();
        let mut filters = Vec::new();
        for arg in path_args {
            if arg.is_empty() {
                // git: an empty pathspec is rejected before any matching.
                eprintln!(
                    "fatal: empty string is not a valid pathspec. please use . instead if you meant to match all paths"
                );
                return Err(GitError::Exit(128));
            }
            let parse_arg = normalize_absolute_cli_pathspec(&root, pathspec_cwd, arg)?;
            let element = parse_normalized_pathspec_element(&prefix, &parse_arg, magic)?;
            // Under literal magic, wildcard characters carry no special meaning.
            let is_glob =
                !element.magic().literal && sley_worktree::pathspec_is_glob(element.pattern());
            let arg_path = Path::new(arg);
            let absolute = if arg_path.is_absolute() {
                arg_path.to_path_buf()
            } else {
                pathspec_cwd.join(arg_path)
            };
            filters.push(LsFilesPathFilter {
                original: arg.clone(),
                recursive: arg == "." || arg.ends_with('/') || absolute.is_dir(),
                is_glob,
                element,
                matched: Cell::new(false),
            });
        }
        let needs_attrs = filters
            .iter()
            .any(|filter| !filter.element.attr_requirements().is_empty());
        let attributes = if needs_attrs {
            Some(sley_worktree::StandardAttributeMatcher::from_worktree_root(
                &root,
            )?)
        } else {
            None
        };
        Ok(Self {
            prefix,
            full_name,
            filters,
            attributes,
        })
    }

    pub(crate) fn untracked_pathspecs(&self) -> Vec<sley_worktree::UntrackedPathspecFilter> {
        self.filters
            .iter()
            .filter(|filter| !filter.is_exclude())
            .map(|filter| sley_worktree::UntrackedPathspecFilter {
                path: filter.element.pattern().to_vec(),
                recursive: filter.recursive,
                is_glob: filter.is_glob,
            })
            .collect()
    }

    pub(crate) fn display(&self, path: &[u8]) -> Option<Vec<u8>> {
        if !self.matches(path) {
            return None;
        }
        if self.full_name || self.prefix.is_empty() {
            return Some(path.to_vec());
        }
        // git renders the matched path relative to the cwd prefix (which it
        // treats as ending in '/'), emitting `../` for each prefix component
        // not shared with `path` — not "up to root then the full path".
        let mut prefix = self.prefix.clone();
        prefix.push(b'/');
        Some(relative_path_bytes(path, &prefix))
    }

    pub(crate) fn matches(&self, path: &[u8]) -> bool {
        if self.filters.is_empty() {
            return self.path_in_default_scope(path);
        }
        let attrs = self.attributes.as_ref();
        let matched = pathspec_filters_match_with(&self.filters, path, |filter, path| {
            filter.matches(path)
                && pathspec_attrs_match_with(&filter.element, |requested| {
                    attribute_checks_for_matching(
                        attrs
                            .map(|matcher| matcher.attributes_for_path(path, requested, false))
                            .unwrap_or_default(),
                    )
                })
        });
        matched
            && (pathspec_filters_have_include(&self.filters) || self.path_in_default_scope(path))
    }

    fn path_in_default_scope(&self, path: &[u8]) -> bool {
        self.full_name
            || self.prefix.is_empty()
            || path
                .strip_prefix(self.prefix.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
                .is_some_and(|rest| !rest.is_empty())
    }

    pub(crate) fn exit_if_unmatched(&self) -> Result<()> {
        let mut has_unmatched = false;
        for filter in &self.filters {
            if !filter.is_exclude() && !filter.matched.get() {
                eprintln!(
                    "error: pathspec '{}' did not match any file(s) known to git",
                    filter.original
                );
                has_unmatched = true;
            }
        }
        if has_unmatched {
            eprintln!("Did you forget to 'git add'?");
            return Err(GitError::Exit(1));
        }
        Ok(())
    }
}

pub(crate) fn normalize_absolute_cli_pathspec(
    root: &Path,
    cwd: &Path,
    arg: &str,
) -> Result<String> {
    let path = Path::new(arg);
    if !path.is_absolute() {
        return Ok(arg.to_string());
    }
    let absolute = fs::canonicalize(path)?;
    let relative = absolute
        .strip_prefix(root)
        .map_err(|_| GitError::InvalidPath(format!("pathspec {arg} is outside worktree")))?;
    let repo_path = relative.to_string_lossy().replace('\\', "/");
    if repo_path.is_empty() {
        return Ok(":/".to_string());
    }
    if cwd == root {
        return Ok(repo_path);
    }
    Ok(format!(":(top){repo_path}"))
}

pub(crate) fn path_component_count(path: &[u8]) -> usize {
    path.split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .count()
}
