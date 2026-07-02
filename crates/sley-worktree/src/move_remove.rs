//! `git rm` / `git mv` over the worktree+index, including submodule `.gitmodules`/gitlink handling.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::filter::*;
use crate::ignore::*;
use crate::index::*;
use crate::index_io::*;
use crate::types_admin::*;

pub fn remove_index_and_worktree_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: RemoveOptions,
    config_parameters_env: Option<&str>,
) -> Result<RemoveResult> {
    let cwd = env::current_dir()?;
    let worktree_root = absolute_path_lexically(worktree_root.as_ref(), &cwd);
    let git_dir = absolute_path_lexically(git_dir.as_ref(), &cwd);
    let worktree_root = worktree_root.as_path();
    let git_dir = git_dir.as_path();
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
    // Stat cache for the local-modification check (git's `ie_match_stat`):
    // proves a path unchanged from the cached stat without reading its blob, so
    // a `git rm --cached` of an untouched path whose blob was removed still
    // succeeds (cf. t1450-fsck cell 90). (`sley_index::IndexStatCache` is a
    // distinct type from this crate's same-named probe helper above.)
    let rm_stat_cache = sley_index::IndexStatCache::from_index(&index, &index_path);
    let Index {
        version: index_version,
        entries: mut index_entry_list,
        extensions: index_extensions,
        ..
    } = index;
    // The set of distinct index paths (any stage) — used for membership tests.
    let index_paths: BTreeSet<Vec<u8>> = index_entry_list
        .iter()
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    let sparse_dir_paths: BTreeSet<Vec<u8>> = index_entry_list
        .iter()
        .filter(|entry| entry.is_sparse_dir())
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    // Paths tracked as a gitlink (mode 160000) at stage 0. Removing one of these
    // from the worktree is a *submodule* removal: git's builtin/rm.c flags the
    // entry `is_submodule = S_ISGITLINK(ce->ce_mode)` and removes the populated
    // submodule *directory* via `remove_dir_recursively` rather than `unlink`,
    // which would fail with EISDIR ("Is a directory") on the submodule checkout.
    // That EISDIR is exactly the gate that blocked the t1013/t7112/t6438/t2013
    // submodule setups. Use the single `sley_index::is_gitlink` rule — no new
    // predicate. (Unmerged gitlinks have no stage-0 entry and are not submodule
    // removals here, matching git, which keys `is_submodule` off the matched ce.)
    let stage0_gitlink_paths: BTreeSet<Vec<u8>> = index_entry_list
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal && sley_index::is_gitlink(entry.mode))
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    let gitlink_paths: BTreeSet<Vec<u8>> = index_entry_list
        .iter()
        .filter(|entry| sley_index::is_gitlink(entry.mode))
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    let gitlink_oids_by_path: BTreeMap<Vec<u8>, BTreeSet<ObjectId>> = {
        let mut by_path: BTreeMap<Vec<u8>, BTreeSet<ObjectId>> = BTreeMap::new();
        for entry in index_entry_list
            .iter()
            .filter(|entry| sley_index::is_gitlink(entry.mode))
        {
            by_path
                .entry(entry.path.as_bytes().to_vec())
                .or_default()
                .insert(entry.oid);
        }
        by_path
    };
    // Paths selected for removal. A single selected path removes ALL of its
    // stage entries (so resolving an unmerged path by removal drops stages
    // 1/2/3 together), matching git's name-keyed removal.
    let mut selected = BTreeSet::new();
    if let Some(mut pathspecs) = RemoveCompiledPathspecs::parse(worktree_root, paths)? {
        for entry in &index_paths {
            if !sparse_dir_paths.contains(entry) && pathspecs.matches(entry) {
                selected.insert(entry.clone());
            }
        }
        pathspecs.require_matched_includes(options.ignore_unmatch)?;
    } else {
        for path in paths {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            // Capture a directory-only pathspec before lexical normalization drops
            // the trailing separator.
            let has_trailing_slash = path_has_trailing_separator(&absolute);
            let absolute = normalize_absolute_path_lexically(&absolute);
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            // A pathspec with a trailing slash (e.g. `git rm dir/`) only matches a
            // directory: it must never match a same-named tracked file.
            let git_path = git_path_bytes(relative)?;
            if !has_trailing_slash && index_paths.contains(&git_path) {
                selected.insert(git_path);
                continue;
            }
            if has_trailing_slash && gitlink_paths.contains(&git_path) && absolute.is_dir() {
                selected.insert(git_path);
                continue;
            }
            // A wildcard pathspec (e.g. `git rm "*"` or `git rm "dir/*.c"`) matches
            // index entries by git's pathspec matcher rather than by literal path or
            // directory prefix. Try the glob match first when the spec contains
            // wildcard metacharacters; a glob match removes the entries directly
            // (no `-r` needed — the pathspec already names the files).
            if pathspec_is_glob(&git_path) {
                let glob_matched = index_paths
                    .iter()
                    .filter(|entry| {
                        pathspec_item_matches(&git_path, entry, PathspecMatchMagic::default())
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if !glob_matched.is_empty() {
                    selected.extend(glob_matched);
                    continue;
                }
                if options.ignore_unmatch {
                    continue;
                }
                eprintln!(
                    "fatal: pathspec '{}' did not match any files",
                    String::from_utf8_lossy(&git_path)
                );
                return Err(GitError::Exit(128));
            }
            let matched = index_paths
                .iter()
                .filter(|entry| {
                    !sparse_dir_paths.contains(*entry)
                        && index_entry_is_under_path(entry, &git_path)
                })
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
    }

    // `git rm` runs the local-modification safety check unless `-f` is given —
    // even for `--cached`. The check (a faithful port of builtin/rm.c's
    // `check_local_mod`) buckets each selected path into one of three error
    // classes and prints all of them at once (collected, not fail-fast), so a
    // single `git rm a b c` reports every offending path. See the message
    // assertions in t3600-rm.sh.
    if !options.force {
        let config =
            sley_config::read_repo_config(git_dir, config_parameters_env).unwrap_or_default();
        // advice.rmhints (default true) gates the parenthetical "(use ...)" hints.
        let show_hints = config.get_bool("advice", None, "rmhints").unwrap_or(true);
        // Map each selected path to its stage-0 index entry for the check; an
        // unmerged path (no stage 0) is skipped, exactly like git's loop
        // (index_name_pos fails, and a non-gitlink ours entry `continue`s).
        let stage0: BTreeMap<&[u8], &IndexEntry> = index_entry_list
            .iter()
            .filter(|entry| entry.stage() == Stage::Normal)
            .map(|entry| (entry.path.as_bytes(), entry))
            .collect();
        let mut files_staged: Vec<&[u8]> = Vec::new();
        let mut files_cached: Vec<&[u8]> = Vec::new();
        let mut files_local: Vec<&[u8]> = Vec::new();
        for path in &selected {
            let Some(index_entry) = stage0.get(path.as_slice()) else {
                // Unmerged ordinary paths are safe to resolve by removal. An
                // unmerged gitlink still needs submodule dirt checks because
                // removing its worktree can discard nested changes.
                if !gitlink_paths.contains(path) {
                    continue;
                }
                if rm_submodule_has_local_changes(
                    worktree_root,
                    format,
                    path,
                    gitlink_oids_by_path.get(path),
                ) {
                    files_local.push(path);
                }
                continue;
            };
            let worktree_file = worktree_path(worktree_root, path)?;
            // Is the worktree path different from the index?
            //
            // Mirror builtin/rm.c's `check_local_mod`: when `lstat` fails with a
            // "missing file" error (ENOENT *or* ENOTDIR — the path vanished, or a
            // leading component became a file) the file has already gone from the
            // working tree, so git `continue`s and never buckets the path. Same
            // for a tracked plain path that is now a directory on disk: git
            // treats that as ENOENT and skips it (the later worktree-removal step
            // is what fails on a non-empty directory).
            let local_changes = if sley_index::is_gitlink(index_entry.mode) {
                rm_submodule_has_local_changes(
                    worktree_root,
                    format,
                    path,
                    gitlink_oids_by_path.get(path),
                )
            } else {
                match fs::symlink_metadata(&worktree_file) {
                    Err(err)
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) || err.raw_os_error() == Some(20) =>
                    {
                        // ENOENT/ENOTDIR: already gone — not warning-worthy.
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                    Ok(meta) if meta.is_dir() => continue,
                    Ok(meta) => {
                        // git refreshes the index before `check_local_mod`, so a path
                        // whose stat changed but whose content is unchanged is up to
                        // date. We mirror that: a clean cached stat short-circuits to
                        // "unchanged"; otherwise re-hash the (clean-filtered) worktree
                        // content and compare to the index entry's *cached oid* (git's
                        // refresh `hash_object`), NOT the stored blob. Comparing to the
                        // oid — not the blob bytes — means a removed object does not
                        // abort the check (the worktree may still hash to the cached
                        // oid), so `git rm --cached` of a path whose blob was deleted
                        // still succeeds.
                        match rm_stat_cache.index_entry_worktree_stat_verdict(index_entry, &meta) {
                            sley_index::StatVerdict::Clean => false,
                            sley_index::StatVerdict::Dirty
                            | sley_index::StatVerdict::RacyNeedsContentCheck => {
                                let worktree_bytes = apply_clean_filter(
                                    worktree_root,
                                    git_dir,
                                    &config,
                                    path,
                                    &fs::read(&worktree_file)?,
                                )?;
                                let worktree_oid =
                                    EncodedObject::new(ObjectType::Blob, worktree_bytes)
                                        .object_id(format)?;
                                worktree_oid != index_entry.oid
                            }
                        }
                    }
                }
            };
            // Is the index different from the HEAD commit? (Before the first
            // commit, anything staged is treated as changed from HEAD.)
            let staged_changes = match head_entries.get(path) {
                Some(head_entry) => {
                    head_entry.oid != index_entry.oid || head_entry.mode != index_entry.mode
                }
                None => true,
            };
            if local_changes && staged_changes {
                // `git rm --cached` of an intent-to-add entry is safe.
                if !options.cached || !index_entry.is_intent_to_add() {
                    files_staged.push(path);
                }
            } else if !options.cached {
                if staged_changes {
                    files_cached.push(path);
                }
                if local_changes {
                    files_local.push(path);
                }
            }
        }
        let mut errs = false;
        print_rm_error_files(
            &files_staged,
            "the following file has staged content different from both the\nfile and the HEAD:",
            "the following files have staged content different from both the\nfile and the HEAD:",
            "\n(use -f to force removal)",
            show_hints,
            &mut errs,
        );
        print_rm_error_files(
            &files_cached,
            "the following file has changes staged in the index:",
            "the following files have changes staged in the index:",
            "\n(use --cached to keep the file, or -f to force removal)",
            show_hints,
            &mut errs,
        );
        print_rm_error_files(
            &files_local,
            "the following file has local modifications:",
            "the following files have local modifications:",
            "\n(use --cached to keep the file, or -f to force removal)",
            show_hints,
            &mut errs,
        );
        if errs {
            return Err(GitError::Exit(1));
        }
    }

    if options.dry_run {
        return Ok(RemoveResult {
            removed: selected.into_iter().collect(),
        });
    }
    let selected_gitlinks = selected
        .iter()
        .filter(|path| gitlink_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    if !options.cached
        && !selected_gitlinks.is_empty()
        && !selected.contains(b".gitmodules".as_slice())
    {
        ensure_gitmodules_clean_for_submodule_rm(
            worktree_root,
            git_dir,
            format,
            &index_entry_list,
            &selected_gitlinks,
            &config_parameters_env,
        )?;
    }
    // Mirror builtin/rm.c's ordering: remove the worktree files BEFORE writing
    // the new index. If the very first removal fails (and nothing has been
    // removed yet), abort without committing the index, so a `git rm d` where
    // `d` is now a non-empty directory fails AND leaves the index untouched.
    // Once any file has been removed we commit to finishing (git does the same).
    if !options.cached {
        let mut removed_any = false;
        for path in &selected {
            let is_gitlink = gitlink_paths.contains(path);
            let is_stage0_gitlink = stage0_gitlink_paths.contains(path);
            match remove_tracked_worktree_path(
                worktree_root,
                path,
                is_gitlink,
                is_stage0_gitlink,
                options.force,
            )? {
                true => removed_any = true,
                false if !removed_any => {
                    eprintln!(
                        "fatal: git rm: '{}': Is a directory",
                        String::from_utf8_lossy(path)
                    );
                    return Err(GitError::Exit(128));
                }
                false => {}
            }
        }
    }
    if !options.cached
        && !selected_gitlinks.is_empty()
        && !selected.contains(b".gitmodules".as_slice())
    {
        remove_submodule_sections_from_gitmodules(
            worktree_root,
            git_dir,
            format,
            &mut index_entry_list,
            &selected_gitlinks,
            &config_parameters_env,
        )?;
    }
    let mut resolve_undo_index = Index {
        version: index_version,
        entries: index_entry_list.clone(),
        extensions: index_extensions,
        checksum: None,
    };
    for path in &selected {
        let range = index_entries_path_range(&resolve_undo_index.entries, path);
        record_resolve_undo_for_range(&mut resolve_undo_index, format, path, range)?;
    }

    // Keep every entry whose path was not selected, preserving original order
    // and all stages of unmerged paths that were not removed.
    let entries = index_entry_list
        .into_iter()
        .filter(|entry| !selected.contains(entry.path.as_bytes()))
        .collect::<Vec<_>>();
    let selected_paths = selected.iter().cloned().collect::<Vec<_>>();
    let mut index = Index {
        version: index_version,
        entries,
        extensions: resolve_undo_index.extensions,
        checksum: None,
    };
    update_cache_tree_for_git_paths(&mut index, format, &db, &selected_paths)?;
    invalidate_untracked_cache_for_git_paths(&mut index, format, &selected_paths)?;
    fs::write(index_path, index.write(format)?)?;
    Ok(RemoveResult {
        removed: selected.into_iter().collect(),
    })
}

struct RemoveCompiledPathspec {
    display: String,
    element: sley_pathspec::PathspecElement,
    matched: bool,
}

struct RemoveCompiledPathspecs {
    specs: Vec<RemoveCompiledPathspec>,
    have_include: bool,
    implicit_prefix: Vec<u8>,
}

impl RemoveCompiledPathspecs {
    fn parse(worktree_root: &Path, paths: &[PathBuf]) -> Result<Option<Self>> {
        let mut saw_magic = false;
        let mut specs = Vec::with_capacity(paths.len());
        let mut have_include = false;
        let mut implicit_prefix = Vec::new();
        for path in paths {
            let parsed = remove_pathspec_parts(worktree_root, path)?;
            saw_magic |= parsed.from_magic;
            if parsed.from_magic && implicit_prefix.is_empty() {
                implicit_prefix = parsed.prefix.clone();
            }
            let element = sley_pathspec::parse_normalized_pathspec_element(
                &parsed.prefix,
                &parsed.raw,
                PathspecMatchMagic::default(),
            )?;
            have_include |= !element.is_exclude();
            specs.push(RemoveCompiledPathspec {
                display: parsed.display,
                element,
                matched: false,
            });
        }
        Ok(saw_magic.then_some(Self {
            specs,
            have_include,
            implicit_prefix,
        }))
    }

    fn matches(&mut self, path: &[u8]) -> bool {
        let mut included = false;
        let mut excluded = false;
        for spec in &mut self.specs {
            if spec.element.matches_path(path) {
                spec.matched = true;
                if spec.element.is_exclude() {
                    excluded = true;
                } else {
                    included = true;
                }
            }
        }
        !excluded
            && if self.have_include {
                included
            } else {
                remove_path_under_prefix(path, &self.implicit_prefix)
            }
    }

    fn require_matched_includes(&self, ignore_unmatch: bool) -> Result<()> {
        if ignore_unmatch {
            return Ok(());
        }
        for spec in &self.specs {
            if !spec.element.is_exclude() && !spec.matched {
                eprintln!("fatal: pathspec '{}' did not match any files", spec.display);
                return Err(GitError::Exit(128));
            }
        }
        Ok(())
    }
}

struct RemovePathspecParts {
    display: String,
    raw: String,
    prefix: Vec<u8>,
    from_magic: bool,
}

fn remove_pathspec_parts(worktree_root: &Path, path: &Path) -> Result<RemovePathspecParts> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        worktree_root.join(path)
    };
    let absolute = normalize_absolute_path_lexically(&absolute);
    let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
    })?;
    let git_path = git_path_bytes(relative)?;
    let components = git_path
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let magic_index = components
        .iter()
        .position(|component| component.starts_with(b":"));
    let Some(magic_index) = magic_index else {
        return Ok(RemovePathspecParts {
            display: path.to_string_lossy().into_owned(),
            raw: String::from_utf8_lossy(&git_path).into_owned(),
            prefix: Vec::new(),
            from_magic: false,
        });
    };
    let prefix = components[..magic_index].join(&b'/');
    let raw = components[magic_index..].join(&b'/');
    Ok(RemovePathspecParts {
        display: path.to_string_lossy().into_owned(),
        raw: String::from_utf8_lossy(&raw).into_owned(),
        prefix,
        from_magic: true,
    })
}

fn remove_path_under_prefix(path: &[u8], prefix: &[u8]) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(b"/"))
}

/// Remove a tracked path from the working tree, mirroring builtin/rm.c's
/// removal loop. For a plain path this is `remove_path`: unlink the file and
/// prune now-empty parent directories. For a gitlink (`is_gitlink`, mode
/// 160000) it is the submodule branch — git removes the populated submodule
/// *directory* with `remove_dir_recursively` (NOT `unlink`, which fails EISDIR),
/// descending into and deleting the nested `.git` because the `git rm` call site
/// passes `flag` *without* `REMOVE_DIR_KEEP_NESTED_GIT`; it `die`s only if that
/// recursive removal genuinely fails.
///
/// Returns `Ok(true)` when the path was removed, `Ok(false)` when a *plain* path
/// could not be unlinked because it is a directory (the caller decides whether
/// that aborts the run). A path that has already vanished is a no-op success.
pub(crate) fn remove_tracked_worktree_path(
    root: &Path,
    path: &[u8],
    is_gitlink: bool,
    is_stage0_gitlink: bool,
    force: bool,
) -> Result<bool> {
    let file = worktree_path(root, path)?;
    match fs::symlink_metadata(&file) {
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(true);
        }
        Err(err) if err.raw_os_error() == Some(20) => return Ok(true), // ENOTDIR
        Err(err) => return Err(err.into()),
        Ok(meta) if meta.is_dir() => {
            if is_gitlink {
                if file.join(".git").is_dir() && !is_stage0_gitlink {
                    return Ok(false);
                }
                if !force && original_cwd_is_inside(&file) {
                    let nested_git = file.join(".git");
                    if nested_git.is_dir() {
                        let _ = fs::remove_dir_all(nested_git);
                    }
                    return Ok(false);
                }
                if contains_nested_git_dir(&file) {
                    eprintln!(
                        "Migrating git directory of '{}' from",
                        String::from_utf8_lossy(path)
                    );
                }
                // Submodule removal. Mirror builtin/rm.c's `is_submodule` branch:
                // `remove_dir_recursively(&buf, force ? REMOVE_DIR_PURGE_ORIGINAL_CWD : 0)`.
                // No `REMOVE_DIR_KEEP_NESTED_GIT` flag, so the whole subtree —
                // including the nested `.git` of the populated submodule — is
                // removed. git `die`s ("could not remove '<path>'") if the
                // recursive removal fails; propagate the IO error to match.
                fs::remove_dir_all(&file)?;
                if fs::symlink_metadata(&file).is_ok() {
                    fs::remove_dir(&file)?;
                }
                prune_empty_parents(root, file.parent())?;
                return Ok(true);
            }
            // A directory in the worktree where a plain file is tracked cannot
            // be unlinked (git's remove_path fails on EISDIR). Report it so the
            // caller can abort the removal without committing the index.
            return Ok(false);
        }
        Ok(_) => {}
    }
    fs::remove_file(&file)?;
    prune_empty_parents(root, file.parent())?;
    Ok(true)
}

pub(crate) fn rm_submodule_has_local_changes(
    worktree_root: &Path,
    format: ObjectFormat,
    path: &[u8],
    expected_oids: Option<&BTreeSet<ObjectId>>,
) -> bool {
    let Ok(submodule_root) = worktree_path(worktree_root, path) else {
        return false;
    };
    if !submodule_root.is_dir() {
        return false;
    }
    let head_changed = sley_diff_merge::gitlink_head_oid(&submodule_root, format)
        .zip(expected_oids)
        .is_some_and(|(head, expected)| !expected.contains(&head));
    head_changed || submodule_dirt(&submodule_root) != 0
}

pub(crate) fn remove_submodule_sections_from_gitmodules(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entries: &mut Vec<IndexEntry>,
    selected_gitlinks: &[Vec<u8>],
    config_parameters_env: &Option<&str>,
) -> Result<()> {
    let gitmodules_path = worktree_root.join(".gitmodules");
    let Ok(original) = fs::read(&gitmodules_path) else {
        return Ok(());
    };
    let gitmodules_index = index_entries.iter().position(|entry| {
        entry.stage() == Stage::Normal && entry.path.as_bytes() == b".gitmodules"
    });
    if gitmodules_index.is_none() {
        return Ok(());
    }
    let config = GitConfig::parse(&original)?;
    let selected = selected_gitlinks
        .iter()
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect::<BTreeSet<_>>();
    let mut sections = Vec::new();
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("submodule") {
            continue;
        }
        let Some(name) = section.subsection.as_deref() else {
            continue;
        };
        let path = section
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key.eq_ignore_ascii_case("path"))
            .and_then(|entry| entry.value.as_deref());
        if path.is_some_and(|path| selected.contains(path)) {
            sections.push(name.to_string());
        }
    }
    let selected_with_sections = sections
        .iter()
        .filter_map(|name| {
            config
                .get("submodule", Some(name), "path")
                .map(ToOwned::to_owned)
        })
        .collect::<BTreeSet<_>>();
    for path in &selected {
        if !selected_with_sections.contains(path) {
            eprintln!("warning: Could not find section in .gitmodules where path={path}");
        }
    }
    if sections.is_empty() {
        return Ok(());
    }
    if gitmodules_worktree_differs_from_index(
        worktree_root,
        git_dir,
        format,
        index_entries,
        &original,
        config_parameters_env,
    )? {
        eprintln!("error: the following file has local modifications:");
        eprintln!("    .gitmodules");
        eprintln!("(use --cached to keep the file, or -f to force removal)");
        return Err(GitError::Exit(1));
    }
    let mut edited = original;
    for name in sections {
        let section_name = format!("submodule.{name}");
        match sley_config::raw_edit::rename_or_remove_section(&edited, &section_name, None) {
            sley_config::raw_edit::SectionEditOutcome::Changed(out) => edited = out,
            sley_config::raw_edit::SectionEditOutcome::NotFound => {
                eprintln!("warning: Could not find section in .gitmodules where path={name}");
            }
            sley_config::raw_edit::SectionEditOutcome::LineTooLong(line) => {
                return Err(GitError::InvalidFormat(format!(
                    "bad config line {line} in .gitmodules"
                )));
            }
        }
    }
    fs::write(&gitmodules_path, &edited)?;
    stage_gitmodules_after_rm(
        worktree_root,
        git_dir,
        format,
        index_entries,
        config_parameters_env,
    )
}

pub(crate) fn ensure_gitmodules_clean_for_submodule_rm(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entries: &[IndexEntry],
    selected_gitlinks: &[Vec<u8>],
    config_parameters_env: &Option<&str>,
) -> Result<()> {
    let gitmodules_path = worktree_root.join(".gitmodules");
    let Ok(original) = fs::read(&gitmodules_path) else {
        return Ok(());
    };
    if !index_entries
        .iter()
        .any(|entry| entry.stage() == Stage::Normal && entry.path.as_bytes() == b".gitmodules")
    {
        return Ok(());
    }
    let config = GitConfig::parse(&original)?;
    let selected = selected_gitlinks
        .iter()
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect::<BTreeSet<_>>();
    let has_matching_section = config.sections.iter().any(|section| {
        section.name.eq_ignore_ascii_case("submodule")
            && section
                .entries
                .iter()
                .rev()
                .find(|entry| entry.key.eq_ignore_ascii_case("path"))
                .and_then(|entry| entry.value.as_deref())
                .is_some_and(|path| selected.contains(path))
    });
    if !has_matching_section {
        return Ok(());
    }
    if gitmodules_worktree_differs_from_index(
        worktree_root,
        git_dir,
        format,
        index_entries,
        &original,
        config_parameters_env,
    )? {
        eprintln!("error: the following file has local modifications:");
        eprintln!("    .gitmodules");
        eprintln!("(use --cached to keep the file, or -f to force removal)");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

pub(crate) fn gitmodules_worktree_differs_from_index(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entries: &[IndexEntry],
    worktree_bytes: &[u8],
    config_parameters_env: &Option<&str>,
) -> Result<bool> {
    let Some(entry) = index_entries
        .iter()
        .find(|entry| entry.stage() == Stage::Normal && entry.path.as_bytes() == b".gitmodules")
    else {
        return Ok(false);
    };
    let config = sley_config::read_repo_config(git_dir, *config_parameters_env).unwrap_or_default();
    let clean = apply_clean_filter(
        worktree_root,
        git_dir,
        &config,
        b".gitmodules",
        worktree_bytes,
    )?;
    let oid = EncodedObject::new(ObjectType::Blob, clean).object_id(format)?;
    Ok(oid != entry.oid)
}

pub(crate) fn stage_gitmodules_after_rm(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entries: &mut [IndexEntry],
    config_parameters_env: &Option<&str>,
) -> Result<()> {
    let path = worktree_root.join(".gitmodules");
    let bytes = fs::read(&path)?;
    let config = sley_config::read_repo_config(git_dir, *config_parameters_env).unwrap_or_default();
    let clean = apply_clean_filter(worktree_root, git_dir, &config, b".gitmodules", &bytes)?;
    let object = EncodedObject::new(ObjectType::Blob, clean);
    let oid = object.object_id(format)?;
    let odb = FileObjectDatabase::from_git_dir(git_dir, format);
    odb.write_object(object)?;
    let metadata = fs::symlink_metadata(&path)?;
    let mut entry =
        index_entry_from_metadata(BString::from(b".gitmodules".as_slice()), oid, &metadata);
    entry.mode = 0o100644;
    if let Some(slot) = index_entries
        .iter_mut()
        .find(|entry| entry.stage() == Stage::Normal && entry.path.as_bytes() == b".gitmodules")
    {
        *slot = entry;
    }
    Ok(())
}

pub(crate) fn prepare_gitmodules_for_moved_gitlinks(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entries: &[IndexEntry],
    moves: &[GitmodulesMove],
) -> Result<Option<Vec<u8>>> {
    if moves.is_empty() {
        return Ok(None);
    }
    let gitmodules_path = worktree_root.join(".gitmodules");
    let Ok(original) = fs::read(&gitmodules_path) else {
        return Ok(None);
    };
    if !index_entries
        .iter()
        .any(|entry| entry.stage() == Stage::Normal && entry.path.as_bytes() == b".gitmodules")
    {
        return Ok(None);
    }
    let config = GitConfig::parse(&original)?;
    let mut edits = Vec::new();
    for gitlink_move in moves {
        let source = String::from_utf8_lossy(&gitlink_move.source).into_owned();
        let destination = String::from_utf8_lossy(&gitlink_move.destination).into_owned();
        let mut matched = false;
        for section in &config.sections {
            if !section.name.eq_ignore_ascii_case("submodule") {
                continue;
            }
            let Some(name) = section.subsection.as_deref() else {
                continue;
            };
            let path = section
                .entries
                .iter()
                .rev()
                .find(|entry| entry.key.eq_ignore_ascii_case("path"))
                .and_then(|entry| entry.value.as_deref());
            if path == Some(source.as_str()) {
                matched = true;
                edits.push((name.to_string(), destination.clone()));
            }
        }
        if !matched {
            eprintln!("warning: Could not find section in .gitmodules where path={source}");
        }
    }
    if edits.is_empty() {
        return Ok(None);
    }
    if gitmodules_worktree_differs_from_index(
        worktree_root,
        git_dir,
        format,
        index_entries,
        &original,
        &None,
    )? {
        eprintln!("fatal: Please stage your changes to .gitmodules or stash them to proceed");
        return Err(GitError::Exit(128));
    }
    let mut edited = original;
    for (name, destination) in edits {
        let mut editor =
            sley_config::raw_edit::RawConfigEditor::new(edited, "submodule", Some(&name), "path");
        match editor.set_multivar(Some(&destination), None, None, false) {
            sley_config::raw_edit::RawEditOutcome::Changed => {}
            sley_config::raw_edit::RawEditOutcome::NothingSet => {
                eprintln!("warning: Could not find section in .gitmodules where path={name}");
            }
        }
        edited = editor.into_bytes();
    }
    Ok(Some(edited))
}

pub(crate) fn apply_prepared_gitmodules_move(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entries: &mut [IndexEntry],
    edited: Vec<u8>,
) -> Result<()> {
    fs::write(worktree_root.join(".gitmodules"), edited)?;
    stage_gitmodules_after_rm(worktree_root, git_dir, format, index_entries, &None)
}

pub(crate) fn prepare_moved_gitlink_gitdirs(
    worktree_root: &Path,
    moves: &[GitmodulesMove],
) -> Result<Vec<GitlinkGitdirMove>> {
    let mut gitdir_moves = Vec::new();
    for gitlink_move in moves {
        let source_root = worktree_path(worktree_root, &gitlink_move.source)?;
        if !source_root.join(".git").is_file() {
            continue;
        }
        let Some(git_dir) = sley_diff_merge::gitlink_git_dir(&source_root) else {
            continue;
        };
        gitdir_moves.push(GitlinkGitdirMove {
            git_dir: normalize_absolute_path_lexically(&git_dir),
            destination_root: worktree_path(worktree_root, &gitlink_move.destination)?,
        });
    }
    Ok(gitdir_moves)
}

pub(crate) fn apply_moved_gitlink_gitdirs(moves: &[GitlinkGitdirMove]) -> Result<()> {
    for gitdir_move in moves {
        let gitdir_relative =
            relative_path_between(&gitdir_move.destination_root, &gitdir_move.git_dir);
        let gitdir_value = gitfile_path_value(&gitdir_relative);
        fs::write(
            gitdir_move.destination_root.join(".git"),
            format!("gitdir: {gitdir_value}\n"),
        )?;

        let config_path = gitdir_move.git_dir.join("config");
        let config_bytes = match fs::read(&config_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err.into()),
        };
        let worktree_relative =
            relative_path_between(&gitdir_move.git_dir, &gitdir_move.destination_root);
        let worktree_value = gitfile_path_value(&worktree_relative);
        let mut editor =
            sley_config::raw_edit::RawConfigEditor::new(config_bytes, "core", None, "worktree");
        match editor.set_multivar(Some(&worktree_value), None, None, false) {
            sley_config::raw_edit::RawEditOutcome::Changed => {
                sley_config::raw_edit::write_config_file_locked(
                    &config_path,
                    &editor.into_bytes(),
                    sley_config::raw_edit::ConfigFileWriteOptions::default(),
                )
                .map_err(|err| GitError::Io(err.to_string()))?;
            }
            sley_config::raw_edit::RawEditOutcome::NothingSet => {}
        }
    }
    Ok(())
}

pub(crate) fn relative_path_between(from_dir: &Path, to_path: &Path) -> PathBuf {
    let from = normalize_absolute_path_lexically(from_dir);
    let to = normalize_absolute_path_lexically(to_path);
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }
    if common == 0 {
        return to;
    }
    let mut relative = PathBuf::new();
    for component in &from_components[common..] {
        if matches!(component, std::path::Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &to_components[common..] {
        match component {
            std::path::Component::Normal(value) => relative.push(value),
            std::path::Component::ParentDir => relative.push(".."),
            std::path::Component::CurDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {}
        }
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}

pub(crate) fn gitfile_path_value(path: &Path) -> String {
    let mut parts = Vec::new();
    let mut absolute = false;
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => {
                parts.push(prefix.as_os_str().to_string_lossy().into_owned());
            }
            std::path::Component::RootDir => absolute = true,
            std::path::Component::CurDir => parts.push(".".to_string()),
            std::path::Component::ParentDir => parts.push("..".to_string()),
            std::path::Component::Normal(value) => {
                parts.push(value.to_string_lossy().into_owned());
            }
        }
    }
    let path = parts.join("/");
    if absolute { format!("/{path}") } else { path }
}

pub(crate) fn contains_nested_git_dir(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name() == ".git" && path.is_dir() {
            return true;
        }
        if path.is_dir() && contains_nested_git_dir(&path) {
            return true;
        }
    }
    false
}

/// Print one batched `git rm` safety error block (mirrors builtin/rm.c's
/// `print_error_files`): the main message, the indented list of offending
/// paths, and — when `advice.rmhints` is enabled — the trailing hint. Sets
/// `*errs` so the caller can fail after collecting every class.
pub(crate) fn print_rm_error_files(
    files: &[&[u8]],
    singular: &str,
    plural: &str,
    hint: &str,
    show_hints: bool,
    errs: &mut bool,
) {
    if files.is_empty() {
        return;
    }
    let mut message = String::from(if files.len() == 1 { singular } else { plural });
    for path in files {
        message.push_str("\n    ");
        message.push_str(&String::from_utf8_lossy(path));
    }
    if show_hints {
        message.push_str(hint);
    }
    eprintln!("error: {message}");
    *errs = true;
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
    let source_absolute = normalize_absolute_path_lexically(&source_absolute);
    let destination_absolute = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        worktree_root.join(destination)
    };
    let destination_has_trailing_separator = path_has_trailing_separator(&destination_absolute);
    let destination_absolute = normalize_absolute_path_lexically(&destination_absolute);
    // When the destination is an existing directory, the source is moved *into*
    // it (`dst/basename`). Record that so the trailing-separator check below does
    // not then reject `git mv file dir/` — git only errors on a trailing slash
    // when the named directory does not exist.
    // A `git mv --sparse` destination may be a directory that is tracked but
    // sparsified off disk (e.g. `mv x folder1` where folder1/ has skip-worktree
    // contents). git still treats it as a directory; detect that from the index.
    let destination_was_existing_dir = destination_absolute.is_dir()
        || (options.sparse
            && move_dir_has_tracked_contents(&index, worktree_root, &destination_absolute));
    let mut destination_absolute = if destination_was_existing_dir {
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
    if path_has_trailing_separator(&destination_absolute)
        && !destination_absolute.exists()
        && source_absolute.is_dir()
        && let (Some(parent), Some(file_name)) = (
            destination_absolute.parent(),
            destination_absolute.file_name(),
        )
    {
        destination_absolute = parent.join(file_name);
    }
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
    if destination_has_trailing_separator
        && !destination_was_existing_dir
        && !destination_absolute.is_dir()
        && !source_absolute.is_dir()
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
    let source_is_conflicted = index.entries.iter().any(|entry| {
        (entry.path.as_bytes() == source_path.as_slice()
            || entry.path.as_bytes().starts_with(&directory_prefix))
            && entry.stage() != Stage::Normal
    });
    if source_is_conflicted {
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
                "fatal: conflicted, source={}, destination={}",
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
            "fatal: conflicted, source={}, destination={}",
            String::from_utf8_lossy(&source_path),
            String::from_utf8_lossy(&destination_path)
        );
        return Err(GitError::Exit(128));
    }
    let source_position = index
        .entries
        .iter()
        .position(|entry| entry.path == source_path && entry.stage() == Stage::Normal);
    let source_is_tracked = !directory_entries.is_empty() || source_position.is_some();
    if !source_is_tracked {
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
    let gitlink_moves = if options.dry_run {
        Vec::new()
    } else if !directory_entries.is_empty() {
        directory_entries
            .iter()
            .filter(|entry| sley_index::is_gitlink(entry.mode))
            .map(|entry| {
                let suffix = &entry.path.as_bytes()[source_path.len()..];
                let mut destination = destination_path.clone();
                destination.extend_from_slice(suffix);
                GitmodulesMove {
                    source: entry.path.as_bytes().to_vec(),
                    destination,
                }
            })
            .collect::<Vec<_>>()
    } else if let Some(position) = source_position {
        let entry = &index.entries[position];
        if sley_index::is_gitlink(entry.mode) {
            vec![GitmodulesMove {
                source: source_path.clone(),
                destination: destination_path.clone(),
            }]
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let gitmodules_move = prepare_gitmodules_for_moved_gitlinks(
        worktree_root,
        git_dir,
        format,
        &index.entries,
        &gitlink_moves,
    )?;
    let gitlink_gitdir_moves = prepare_moved_gitlink_gitdirs(worktree_root, &gitlink_moves)?;
    if options.sparse && !directory_entries.is_empty() {
        return sparse_directory_move(
            worktree_root,
            git_dir,
            format,
            source_path,
            destination_path,
            &directory_prefix,
            directory_entries,
            index,
            options.force,
            gitmodules_move,
            &gitlink_gitdir_moves,
        );
    }
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
        apply_moved_gitlink_gitdirs(&gitlink_gitdir_moves)?;
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
        if let Some(edited) = gitmodules_move {
            apply_prepared_gitmodules_move(
                worktree_root,
                git_dir,
                format,
                &mut index.entries,
                edited,
            )?;
        }
        index
            .entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        index.extensions.clear();
        write_repository_index_ref(git_dir, format, &index)?;
        return Ok(MoveResult {
            source: source_path,
            destination: destination_path,
            skipped: false,
            fatal: None,
            details,
        });
    }

    let position = source_position.expect("tracked non-directory source must have an index entry");
    if options.dry_run {
        return Ok(MoveResult {
            source: source_path,
            destination: destination_path,
            skipped: false,
            fatal: None,
            details: Vec::new(),
        });
    }
    // `git mv --sparse` of a single file reconciles the worktree with the
    // destination's cone membership instead of doing a plain rename: a
    // skip-worktree source has no on-disk file to move, and the destination is
    // materialized (and the bit cleared) only when it lands inside the cone.
    if options.sparse {
        return sparse_single_file_move(
            worktree_root,
            git_dir,
            format,
            &source_absolute,
            &destination_absolute,
            source_path,
            destination_path,
            index,
            position,
            options.force,
            gitmodules_move,
            &gitlink_gitdir_moves,
        );
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
    apply_moved_gitlink_gitdirs(&gitlink_gitdir_moves)?;
    let source_entry = index.entries.remove(position);
    let mut destination_entry = source_entry;
    destination_entry.path = destination_path.clone().into();
    destination_entry.refresh_name_length();
    index.entries.retain(|entry| entry.path != destination_path);
    index.entries.push(destination_entry);
    if let Some(edited) = gitmodules_move {
        apply_prepared_gitmodules_move(worktree_root, git_dir, format, &mut index.entries, edited)?;
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    index.extensions.clear();
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(MoveResult {
        source: source_path,
        destination: destination_path,
        skipped: false,
        fatal: None,
        details: Vec::new(),
    })
}

/// Whether the index holds any tracked entries under `dir_absolute`. Used to
/// recognise a directory that `git sparse-checkout` removed from disk but still
/// tracks as a valid `git mv --sparse` destination directory.
fn move_dir_has_tracked_contents(index: &Index, worktree_root: &Path, dir_absolute: &Path) -> bool {
    let Ok(relative) = dir_absolute.strip_prefix(worktree_root) else {
        return false;
    };
    let Ok(git_path) = git_path_bytes(relative) else {
        return false;
    };
    if git_path.is_empty() {
        return false;
    }
    let mut prefix = git_path;
    prefix.push(b'/');
    index
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes().starts_with(&prefix))
}

/// `git mv --sparse` of a single tracked file. Rather than renaming on disk
/// (the source may be a skip-worktree entry with no worktree file), this moves
/// the index entry and reconciles the worktree + skip-worktree bit with the
/// destination's sparse-checkout cone membership: in-cone destinations are
/// materialized with the bit cleared; out-of-cone destinations keep no worktree
/// file and gain the skip-worktree bit (git's mv.c SPARSE handling).
#[allow(clippy::too_many_arguments)]
fn sparse_single_file_move(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    source_absolute: &Path,
    destination_absolute: &Path,
    source_path: Vec<u8>,
    destination_path: Vec<u8>,
    mut index: Index,
    position: usize,
    force: bool,
    gitmodules_move: Option<Vec<u8>>,
    gitlink_gitdir_moves: &[GitlinkGitdirMove],
) -> Result<MoveResult> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let (cone_mode, destination_in_cone) = match crate::checkout::active_sparse_checkout(git_dir)? {
        Some((sparse, mode)) => (
            matches!(mode, SparseCheckoutMode::Cone),
            crate::checkout::path_in_sparse_checkout(&destination_path, &sparse, mode),
        ),
        None => (false, true),
    };
    // git refuses to overwrite a destination that already exists in the index
    // when it is out-of-cone, unless --force is given (builtin/mv.c's
    // "destination exists in the index"). This is gated on cone mode: dst_mode's
    // SPARSE/SKIP_WORKTREE_DIR bits are only assigned for cone-mode checkouts.
    if cone_mode
        && !destination_in_cone
        && !force
        && index.entries.iter().any(|entry| {
            entry.path.as_bytes() == destination_path.as_slice() && entry.stage() == Stage::Normal
        })
    {
        eprintln!(
            "fatal: destination exists in the index, source={}, destination={}",
            String::from_utf8_lossy(&source_path),
            String::from_utf8_lossy(&destination_path)
        );
        return Err(GitError::Exit(128));
    }
    let source_present = fs::symlink_metadata(source_absolute).is_ok();
    // A dirty source moving out-of-cone keeps its worktree file (and stays
    // cached) instead of being discarded; detect that before mutating the index.
    let source_dirty = source_present && cone_mode && !destination_in_cone && {
        let index_path = repository_index_path(git_dir);
        let stat_cache = IndexStatCache::from_index(&index, &index_path);
        let mut clean_filter = None;
        mv_source_is_dirty(
            worktree_root,
            git_dir,
            format,
            &index.entries[position],
            &stat_cache,
            &mut clean_filter,
        )?
    };
    let mut dirty_paths = Vec::new();
    let mut destination_entry = index.entries.remove(position);
    destination_entry.path = destination_path.clone().into();
    destination_entry.refresh_name_length();
    index.entries.retain(|entry| entry.path != destination_path);
    // git only re-homes the worktree file and toggles the skip-worktree bit for
    // cone-mode transitions (builtin/mv.c gates this on
    // core_sparse_checkout_cone); everything else is a plain rename that
    // preserves the bit.
    if source_present {
        if cone_mode && !destination_in_cone {
            if source_dirty {
                // Dirty in-cone -> out-of-cone: move the file to the destination
                // and keep it cached (no skip-worktree); warn about the dirty path.
                if let Some(parent) = destination_absolute.parent() {
                    fs::create_dir_all(parent)?;
                }
                if fs::symlink_metadata(destination_absolute).is_ok() {
                    crate::checkout::remove_existing_worktree_path(destination_absolute)?;
                }
                fs::rename(source_absolute, destination_absolute)?;
                if let Ok(metadata) = fs::symlink_metadata(destination_absolute) {
                    destination_entry =
                        index_entry_with_refreshed_stat(&destination_entry, &metadata);
                }
                dirty_paths.push(destination_path.clone());
            } else {
                // Clean in-cone -> out-of-cone: drop the worktree file, keep only
                // the (now skip-worktree) index entry.
                crate::checkout::remove_existing_worktree_path(source_absolute)?;
                if fs::symlink_metadata(destination_absolute).is_ok() {
                    crate::checkout::remove_existing_worktree_path(destination_absolute)?;
                }
                crate::checkout::set_skip_worktree(&mut destination_entry);
            }
        } else {
            // Plain rename of the present worktree file.
            if let Some(parent) = destination_absolute.parent() {
                fs::create_dir_all(parent)?;
            }
            if fs::symlink_metadata(destination_absolute).is_ok() {
                crate::checkout::remove_existing_worktree_path(destination_absolute)?;
            }
            fs::rename(source_absolute, destination_absolute)?;
            if destination_in_cone {
                crate::checkout::clear_skip_worktree(&mut destination_entry);
            }
            if let Ok(metadata) = fs::symlink_metadata(destination_absolute) {
                destination_entry = index_entry_with_refreshed_stat(&destination_entry, &metadata);
            }
        }
    } else if cone_mode && destination_in_cone {
        // Sparse (skip-worktree) source moving into the cone: materialize it.
        crate::checkout::clear_skip_worktree(&mut destination_entry);
        if fs::symlink_metadata(destination_absolute).is_err() {
            crate::checkout::materialize_index_entry_file(
                &db,
                worktree_root,
                destination_absolute,
                &destination_entry,
            )?;
        }
        if let Ok(metadata) = fs::symlink_metadata(destination_absolute) {
            destination_entry = index_entry_with_refreshed_stat(&destination_entry, &metadata);
        }
    }
    // Otherwise (out-of-cone -> out-of-cone, or any non-cone move of an absent
    // source) the index entry simply moves and keeps its skip-worktree bit.
    index.entries.push(destination_entry);
    apply_moved_gitlink_gitdirs(gitlink_gitdir_moves)?;
    if let Some(edited) = gitmodules_move {
        apply_prepared_gitmodules_move(worktree_root, git_dir, format, &mut index.entries, edited)?;
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    index.extensions.clear();
    write_repository_index_ref(git_dir, format, &index)?;
    advise_on_moving_dirty_paths(git_dir, &dirty_paths);
    Ok(MoveResult {
        source: source_path,
        destination: destination_path,
        skipped: false,
        fatal: None,
        details: Vec::new(),
    })
}

/// Whether a tracked entry's worktree file differs from the index (git's
/// `ie_modified`), so a `git mv --sparse` out-of-cone move must keep the file
/// rather than discard it.
fn mv_source_is_dirty(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    entry: &IndexEntry,
    stat_cache: &IndexStatCache,
    clean_filter: &mut Option<TrackedOnlyCleanFilter>,
) -> Result<bool> {
    let worktree_entry = worktree_entry_for_index_entry_with_attributes(
        worktree_root,
        git_dir,
        format,
        entry,
        stat_cache,
        clean_filter,
    )?;
    Ok(match worktree_entry {
        Some(worktree_entry) => {
            worktree_entry.oid != entry.oid || worktree_entry.mode != entry.mode
        }
        None => false,
    })
}

/// git's `advise_on_moving_dirty_path`: the "moved outside the sparse-checkout
/// definition but are not sparse due to local modifications" header, the dirty
/// paths, then the hint block (gated by `advice.updateSparsePath`).
fn advise_on_moving_dirty_paths(git_dir: &Path, paths: &[Vec<u8>]) {
    if paths.is_empty() {
        return;
    }
    eprintln!("The following paths have been moved outside the");
    eprintln!("sparse-checkout definition but are not sparse due to local");
    eprintln!("modifications.");
    for path in paths {
        eprintln!("{}", String::from_utf8_lossy(path));
    }
    let show_hint = sley_config::read_repo_config(git_dir, None)
        .ok()
        .and_then(|config| config.get_bool("advice", None, "updateSparsePath"))
        .unwrap_or(true);
    if show_hint {
        eprintln!("hint: To correct the sparsity of these paths, do the following:");
        eprintln!("hint: * Use \"git add --sparse <paths>\" to update the index");
        eprintln!("hint: * Use \"git sparse-checkout reapply\" to apply the sparsity rules");
        eprintln!(
            "hint: Disable this message with \"git config set advice.updateSparsePath false\""
        );
    }
}

/// Recursively remove `dir` and its subdirectories when they contain only
/// (possibly nested) empty directories. Returns whether `dir` was removed.
fn remove_empty_dirs_under(dir: &Path) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let mut all_empty = true;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if !remove_empty_dirs_under(&path)? {
                all_empty = false;
            }
        } else {
            all_empty = false;
        }
    }
    if all_empty {
        fs::remove_dir(dir)?;
    }
    Ok(all_empty)
}

/// `git mv --sparse` of a directory: reconcile each contained entry with the
/// destination's cone membership the same way `sparse_single_file_move` does a
/// single file, then prune now-empty source directories. The source directory
/// may be sparsified off disk, so this never relies on a whole-tree rename.
#[allow(clippy::too_many_arguments)]
fn sparse_directory_move(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    source_path: Vec<u8>,
    destination_path: Vec<u8>,
    directory_prefix: &[u8],
    directory_entries: Vec<IndexEntry>,
    mut index: Index,
    force: bool,
    gitmodules_move: Option<Vec<u8>>,
    gitlink_gitdir_moves: &[GitlinkGitdirMove],
) -> Result<MoveResult> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let active = crate::checkout::active_sparse_checkout(git_dir)?;
    let cone_mode = matches!(
        active.as_ref().map(|(_, mode)| mode),
        Some(SparseCheckoutMode::Cone)
    );
    let path_in_cone = |path: &[u8]| match active.as_ref() {
        Some((sparse, mode)) => crate::checkout::path_in_sparse_checkout(path, sparse, *mode),
        None => true,
    };
    let index_path = repository_index_path(git_dir);
    let stat_cache = IndexStatCache::from_index(&index, &index_path);
    let mut clean_filter = None;
    let mut dirty_paths = Vec::new();
    let mut details = Vec::new();
    let mut new_entries = Vec::new();
    let mut source_parents: Vec<PathBuf> = Vec::new();
    for entry in &directory_entries {
        let suffix = &entry.path.as_bytes()[source_path.len()..];
        let mut destination = destination_path.clone();
        destination.extend_from_slice(suffix);
        let destination_in_cone = path_in_cone(&destination);
        if cone_mode
            && !destination_in_cone
            && !force
            && index.entries.iter().any(|existing| {
                existing.path.as_bytes() == destination.as_slice()
                    && existing.stage() == Stage::Normal
                    && !existing.path.as_bytes().starts_with(directory_prefix)
            })
        {
            eprintln!(
                "fatal: destination exists in the index, source={}, destination={}",
                String::from_utf8_lossy(entry.path.as_bytes()),
                String::from_utf8_lossy(&destination)
            );
            return Err(GitError::Exit(128));
        }
        let mut destination_entry = entry.clone();
        destination_entry.path = destination.clone().into();
        destination_entry.refresh_name_length();
        let source_absolute = worktree_path(worktree_root, entry.path.as_bytes())?;
        let destination_absolute = worktree_path(worktree_root, &destination)?;
        if let Some(parent) = source_absolute.parent() {
            source_parents.push(parent.to_path_buf());
        }
        let source_present = fs::symlink_metadata(&source_absolute).is_ok();
        if source_present {
            if cone_mode && !destination_in_cone {
                let dirty = mv_source_is_dirty(
                    worktree_root,
                    git_dir,
                    format,
                    entry,
                    &stat_cache,
                    &mut clean_filter,
                )?;
                if dirty {
                    // Dirty file: keep it at the destination, stay cached.
                    if let Some(parent) = destination_absolute.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    if fs::symlink_metadata(&destination_absolute).is_ok() {
                        crate::checkout::remove_existing_worktree_path(&destination_absolute)?;
                    }
                    fs::rename(&source_absolute, &destination_absolute)?;
                    if let Ok(metadata) = fs::symlink_metadata(&destination_absolute) {
                        destination_entry =
                            index_entry_with_refreshed_stat(&destination_entry, &metadata);
                    }
                    dirty_paths.push(destination.clone());
                } else {
                    crate::checkout::remove_existing_worktree_path(&source_absolute)?;
                    if fs::symlink_metadata(&destination_absolute).is_ok() {
                        crate::checkout::remove_existing_worktree_path(&destination_absolute)?;
                    }
                    crate::checkout::set_skip_worktree(&mut destination_entry);
                }
            } else {
                if let Some(parent) = destination_absolute.parent() {
                    fs::create_dir_all(parent)?;
                }
                if fs::symlink_metadata(&destination_absolute).is_ok() {
                    crate::checkout::remove_existing_worktree_path(&destination_absolute)?;
                }
                fs::rename(&source_absolute, &destination_absolute)?;
                if destination_in_cone {
                    crate::checkout::clear_skip_worktree(&mut destination_entry);
                }
                if let Ok(metadata) = fs::symlink_metadata(&destination_absolute) {
                    destination_entry =
                        index_entry_with_refreshed_stat(&destination_entry, &metadata);
                }
            }
        } else if cone_mode && destination_in_cone {
            crate::checkout::clear_skip_worktree(&mut destination_entry);
            if fs::symlink_metadata(&destination_absolute).is_err() {
                crate::checkout::materialize_index_entry_file(
                    &db,
                    worktree_root,
                    &destination_absolute,
                    &destination_entry,
                )?;
            }
            if let Ok(metadata) = fs::symlink_metadata(&destination_absolute) {
                destination_entry = index_entry_with_refreshed_stat(&destination_entry, &metadata);
            }
        }
        details.push(MoveDetail {
            source: entry.path.as_bytes().to_vec(),
            destination: destination.clone(),
            skipped: false,
        });
        new_entries.push(destination_entry);
    }
    let destination_paths: Vec<Vec<u8>> = new_entries
        .iter()
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    index.entries.retain(|entry| {
        !entry.path.as_bytes().starts_with(directory_prefix)
            && !destination_paths
                .iter()
                .any(|path| path.as_slice() == entry.path.as_bytes())
    });
    index.entries.extend(new_entries);
    // git's remove_empty_src_dirs strips directories the move emptied, including
    // untracked empty subdirectories left behind under the source root.
    let source_root = worktree_path(worktree_root, &source_path)?;
    remove_empty_dirs_under(&source_root)?;
    for parent in source_parents {
        prune_empty_parents(worktree_root, Some(&parent))?;
    }
    advise_on_moving_dirty_paths(git_dir, &dirty_paths);
    apply_moved_gitlink_gitdirs(gitlink_gitdir_moves)?;
    if let Some(edited) = gitmodules_move {
        apply_prepared_gitmodules_move(worktree_root, git_dir, format, &mut index.entries, edited)?;
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    index.extensions.clear();
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(MoveResult {
        source: source_path,
        destination: destination_path,
        skipped: false,
        fatal: None,
        details,
    })
}
