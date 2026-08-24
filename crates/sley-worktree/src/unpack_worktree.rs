//! The working-tree side of unpack-trees transitions (`ReadTreeWorktree`).
//!
//! This is git's `unpack-trees.c` I/O layer: the [`sley_unpack_trees::WorktreeProbe`]
//! / [`sley_unpack_trees::WorktreeWriter`] implementation over a real filesystem
//! and index, plus the two-way checkout driver behind `git checkout <branch>`,
//! `git switch`, and `git reset --keep`. It was moved here verbatim from the CLI
//! `read-tree` command so the published engine crates own the reusable plumbing;
//! the porcelain keeps thin call wrappers.

use super::*;
use crate::index_io::{original_cwd_absolute, path_is_original_cwd};
use std::io;

/// Which command's porcelain error strings the engine's safety checks should
/// emit, mirroring git's `setup_unpack_trees_porcelain(o, cmd)`. The merge
/// rules are identical across commands; only the *user-facing abort text*
/// differs ("...by checkout" vs "...by merge", and the trailing
/// "switch branches" vs "merge" hint). `checkout <branch>` / `switch` /
/// `checkout --detach` all use [`UnpackPorcelain::Checkout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpackPorcelain {
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

/// Porcelain-provided submodule transition hooks. The unpack worktree probe and
/// writer delegate recursive submodule materialization/removal to these so the
/// engine stays free of clone/session machinery; porcelain passes closures that
/// wrap its own submodule drivers.
pub type SubmoduleCheckoutHook<'a> =
    &'a dyn Fn(&Path, &Path, ObjectFormat, &[u8], &ObjectId) -> Result<()>;
pub type SubmoduleRemoveHook<'a> = &'a dyn Fn(&Path, &Path, &[u8]) -> Result<()>;

#[derive(Clone, Copy, Default)]
pub struct SubmoduleHooks<'a> {
    pub checkout_to_commit: Option<SubmoduleCheckoutHook<'a>>,
    pub remove_worktree: Option<SubmoduleRemoveHook<'a>>,
}

/// The working-tree side of a `-m` merge, supplying the `sley-unpack-trees`
/// engine with read-tree's I/O: how to tell whether a path is up to date
/// (hashing the worktree blob), whether materializing/removing a path would
/// clobber an untracked file, and how to write/remove worktree files when `-u`
/// applies the result.
pub struct ReadTreeWorktree<'a> {
    pub worktree_root: PathBuf,
    pub git_dir: PathBuf,
    pub db: &'a FileObjectDatabase,
    pub format: ObjectFormat,
    /// Every path present in the *pre-merge* index (any stage). A merged-result
    /// path not in this set is a fresh addition, so writing it must not clobber
    /// an untracked working-tree file.
    pub original_paths: BTreeSet<Vec<u8>>,
    /// Typed `.gitmodules` of the superproject worktree, parsed once. `None`
    /// when there is no `.gitmodules` (then no path is a submodule and the
    /// move-head hook is a no-op). This is git's `submodule_from_ce` source.
    pub submodules: Option<sley_submodule::SubmoduleConfigSet>,
    /// The superproject's `.git/config`, parsed once, for the
    /// `is_submodule_active` resolution (`submodule.<name>.active` /
    /// `submodule.active` / `submodule.<name>.url`).
    pub repo_config: GitConfig,
    /// Attribute stack from the target tree. Workers receive already-converted
    /// bytes and never consult a concurrently changing worktree stack.
    pub tree_attributes: Option<TreeAttributes>,
    /// Which command's abort text the safety checks emit (git's
    /// `setup_unpack_trees_porcelain`). `read-tree` keeps its historic
    /// per-path messages; `checkout`/`switch` use the collected-path block.
    pub porcelain: UnpackPorcelain,
    /// Whether tree application should run the submodule move-head mutation path
    /// rather than only creating/removing the gitlink directory placeholder.
    pub recurse_submodules: bool,
    /// Force checkout/reset mode: tracked worktree modifications may be
    /// overwritten, so `verify_uptodate` must not reject them.
    pub force_overwrite_tracked: bool,
    /// Porcelain-supplied submodule transition drivers (recursive checkout and
    /// worktree removal). Without a hook the writer falls back to plain
    /// gitlink placeholder handling.
    pub hooks: SubmoduleHooks<'a>,
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
        // git's `check_leading_path` (symlinks.c): walk each leading component
        // with lstat. When a non-directory (symlink or regular file) sits on
        // the path, `lstat(full_path)` can still "succeed" by following the
        // intermediate symlink (e.g. tracked `frotz` → `xyzzy` makes
        // `frotz/filfre` appear as `xyzzy/filfre`). Git then only checks that
        // *leading* component: if it is tracked (will be CE_REMOVE'd by the
        // merge, as in the symlink→dir D/F case of t2007) the path is free;
        // if untracked it is the obstruction. Never report the through-symlink
        // leaf as an untracked file.
        if let Some(leading) = leading_nondir_component(&self.worktree_root, path)? {
            if self.original_paths.contains(leading.as_slice()) {
                return Ok(());
            }
            return reject_untracked_would_be_overwritten(self.porcelain, &leading);
        }
        let Ok(metadata) = fs::symlink_metadata(&file_path) else {
            return Ok(());
        };
        if path_matches_standard_ignore(&self.worktree_root, path, metadata.is_dir())? {
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
        reject_untracked_would_be_overwritten(self.porcelain, path)
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

    fn sparse_checkout_path_present(&self, path: &[u8]) -> Result<bool> {
        let Some(file_path) = safe_worktree_path(&self.worktree_root, path) else {
            return Ok(false);
        };
        let Ok(metadata) = fs::symlink_metadata(&file_path) else {
            return Ok(false);
        };
        Ok(!path_matches_standard_ignore(
            &self.worktree_root,
            path,
            metadata.is_dir(),
        )?)
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
    /// no **untracked, not-ignored** file (one absent from the pre-merge index
    /// and excluded by neither `.gitignore` nor `.git/info/exclude`). Upstream
    /// collects the survivors with `read_directory`, whose standard exclude
    /// machinery drops ignored paths, so ignored files (build output, caches)
    /// do not block the replacement and are removed with the subtree. Every
    /// file under it that *is* tracked is already accounted for by the merge
    /// result (it will be removed or rewritten), so it does not block either.
    ///
    /// On a clean subdirectory the writer's `remove_subtree` clears it before the
    /// file is written; on an unclean one this rejects with git's
    /// `ERROR_NOT_UPTODATE_DIR` exit so no untracked work is silently destroyed.
    fn verify_clean_subdirectory(&self, dir_git_path: &[u8], dir_fs_path: &Path) -> Result<()> {
        if original_cwd_relative_to(&self.worktree_root).as_deref() == Some(dir_git_path) {
            return refuse_remove_current_working_directory(dir_git_path);
        }
        // One matcher for the whole walk: git builds a single `dir_struct` per
        // call too (`read_directory`), sharing the exclude-per-directory stack.
        let ignores = crate::ignore::IgnoreMatcher::from_worktree_root(&self.worktree_root)?;
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
                    // An ignored subdirectory is pruned wholesale by
                    // `read_directory`; nothing under it can block.
                    if !ignores.is_ignored(&child_git, true) {
                        stack.push((entry.path(), child_git));
                    }
                    continue;
                }
                // A tracked path (present in the pre-merge index at any stage) is
                // owned by the merge; an untracked one would be lost → reject,
                // unless the standard ignore rules excuse it.
                if !self.original_paths.contains(&child_git)
                    && !ignores.is_ignored(&child_git, false)
                {
                    match self.porcelain {
                        // read-tree keeps its historic per-path wording and dies.
                        UnpackPorcelain::ReadTree => {
                            let display = String::from_utf8_lossy(dir_git_path);
                            eprintln!(
                                "error: Updating '{display}' would lose untracked files in it"
                            );
                        }
                        // The checkout/switch porcelain uses the collected-path
                        // block from setup_unpack_trees_porcelain and exits 1.
                        UnpackPorcelain::Checkout => {
                            let display = String::from_utf8_lossy(dir_git_path);
                            eprintln!(
                                "error: Updating the following directories would lose untracked files in them:"
                            );
                            eprintln!("\t{display}");
                            eprintln!();
                            eprintln!("Aborting");
                        }
                    }
                    return Err(GitError::Exit(unpack_rejection_exit(self.porcelain)));
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
        write_tree_entry_to_worktree_with_hooks(
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
            self.hooks,
        )
    }

    fn write_blobs(
        &mut self,
        entries: &[(Vec<u8>, u32, ObjectId)],
    ) -> Result<Vec<Option<sley_unpack_trees::StatInfo>>> {
        let ordinary = entries
            .iter()
            .filter(|(_, mode, _)| !sley_index::is_gitlink(*mode))
            .map(
                |(path, mode, oid)| CheckoutMaterializationEntry {
                    path: path.clone(),
                    mode: *mode,
                    oid: *oid,
                },
            )
            .collect::<Vec<_>>();
        let mut ordinary_stats =
            materialize_checkout_entries_with_database(
                &self.worktree_root,
                &self.git_dir,
                self.format,
                self.db,
                &self.repo_config,
                self.tree_attributes.as_ref(),
                &ordinary,
            )?
            .stats;
        entries
            .iter()
            .map(|(path, mode, oid)| {
                if sley_index::is_gitlink(*mode) {
                    return self.write_blob(path, *mode, oid);
                }
                ordinary_stats.remove(path).ok_or_else(|| {
                    GitError::Transaction(format!(
                        "checkout worker did not report path '{}'",
                        String::from_utf8_lossy(path)
                    ))
                })
            })
            .collect()
    }

    fn remove_path(&mut self, path: &[u8]) -> Result<()> {
        if self.recurse_submodules && self.path_is_configured_submodule(path) {
            if let Some(remove_worktree) = self.hooks.remove_worktree {
                return remove_worktree(&self.worktree_root, &self.git_dir, path);
            }
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
/// `repo_config` is the invocation-effective repository configuration (the
/// caller's config view, including `-c` overrides); it feeds smudge filters,
/// submodule activity resolution, and attribute stacks.
///
/// `porcelain` selects the abort wording (git's
/// `setup_unpack_trees_porcelain`): `checkout`/`switch` use
/// [`UnpackPorcelain::Checkout`]; `reset --keep`, which runs the identical
/// `twoway_merge` from `reset.c`, uses [`UnpackPorcelain::ReadTree`] (the
/// per-path `Entry '...' not uptodate. Cannot merge.` message its test asserts).
#[allow(clippy::too_many_arguments)]
pub fn checkout_two_way_engine(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    repo_config: &GitConfig,
    old_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
    porcelain: UnpackPorcelain,
    recurse_submodules: bool,
    overwrite_untracked: bool,
) -> Result<()> {
    let previous_index = read_repository_index(git_dir, format)?;
    let mut index = read_current_unpack_index(git_dir, format)?;

    // git's `merge_working_tree`: `trees[0]` = the tree of the HEAD being left
    // (empty when HEAD is unborn), `trees[1]` = the tree being checked out.
    let old_leaves = match old_tree {
        Some(oid) => Some(sley_diff_merge::flatten_tree(db, format, oid)?),
        None => None,
    };
    let new_leaves = sley_diff_merge::flatten_tree(db, format, new_tree)?;
    let mut sparse_paths = index.keys().cloned().collect::<BTreeSet<_>>();
    if let Some(old_leaves) = old_leaves.as_ref() {
        sparse_paths.extend(old_leaves.keys().cloned());
    }
    sparse_paths.extend(new_leaves.keys().cloned());
    let apply_sparse_checkout =
        configure_active_sparse_checkout_for_unpack_index(git_dir, &mut index, &sparse_paths)?;

    let tree_attributes =
        TreeAttributes::from_tree(worktree_root, git_dir, db, format, new_tree)?;
    let mut wt = ReadTreeWorktree {
        submodules: load_superproject_submodules(worktree_root),
        repo_config: repo_config.clone(),
        tree_attributes: Some(tree_attributes),
        worktree_root: worktree_root.to_path_buf(),
        git_dir: git_dir.to_path_buf(),
        db,
        format,
        original_paths: read_current_index_paths(git_dir, format)?,
        porcelain,
        recurse_submodules,
        force_overwrite_tracked: overwrite_untracked,
        hooks: SubmoduleHooks::default(),
    };

    // git's `merge_working_tree` runs the merge to *populate the result* with
    // every up-to-date / clobber rejection collected first, then applies the
    // worktree side only if nothing was rejected. `unpack_trees` here aborts on
    // the first rejection (before `check_updates` touches the worktree), so a
    // failed checkout leaves the working tree exactly as it was — matching git's
    // "Aborting" guarantee.
    let mut options = sley_unpack_trees::CheckoutTransitionOptions::new(format);
    options.overwrite_untracked = overwrite_untracked;
    options.apply_sparse_checkout = apply_sparse_checkout;
    let plan =
        sley_unpack_trees::plan_checkout_transition(&index, old_leaves, new_leaves, options, &wt)?;
    refuse_if_unpack_result_removes_current_directory(worktree_root, plan.result())?;
    let result = plan.apply(&mut wt)?;
    if !result.sparse_checkout_present_paths.is_empty() {
        eprintln!(
            "warning: The following paths were already present and thus not updated despite sparse patterns:"
        );
        for path in &result.sparse_checkout_present_paths {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!();
        eprintln!(
            "After fixing the above paths, you may want to run `git sparse-checkout reapply`."
        );
    }

    // Serialize the merged index. check_updates folded the post-write `lstat`
    // back into each freshly-written entry, so the stat info is accurate.
    let pairs: Vec<(Vec<u8>, ReadTreeEntry)> = result
        .entries
        .into_iter()
        .map(|e| {
            (
                e.path,
                ReadTreeEntry {
                    mode: e.entry.mode,
                    oid: e.entry.oid,
                    stage: e.entry.stage,
                    stat: e.entry.stat,
                    skip_worktree: Some(e.entry.is_skip_worktree()),
                },
            )
        })
        .collect();
    match previous_index.as_ref() {
        Some(previous) => persist_checkout_read_tree_entries(git_dir, format, pairs, previous),
        None => persist_read_tree_entries(git_dir, format, pairs),
    }
}

/// Parse the superproject's `.gitmodules` into the typed config set (git's
/// `submodule_from_path` source). `None` when there is no `.gitmodules`, in
/// which case no path is a submodule and the move-head hook never fires.
pub fn load_superproject_submodules(
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
        return crate::discovery::read_gitdir_link(&dot_git)
            .ok()
            .flatten()
            .is_some();
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
    let status = Command::new(exe)
        .args(["diff-index", "--quiet", "--cached", "HEAD"])
        .current_dir(sub_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
pub fn verify_uptodate_path(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    path: &[u8],
    expected: Option<&(u32, ObjectId)>,
    porcelain: UnpackPorcelain,
) -> Result<()> {
    let Some((expected_mode, expected_oid)) = expected else {
        // Untracked path: nothing in the index to be out of date with.
        return Ok(());
    };
    let state = worktree_entry_state_by_git_path(
        worktree_root,
        git_dir,
        format,
        path,
        expected_oid,
        *expected_mode,
        None,
    )?;
    if state == WorktreeEntryState::Modified {
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
pub fn safe_worktree_path(root: &Path, path: &[u8]) -> Option<PathBuf> {
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
    tree_attributes: Option<&TreeAttributes>,
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
            None => apply_smudge_filter(worktree_root, git_dir, format, config, path, &object.body)?,
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
pub fn write_tree_entry_to_worktree(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    tree_attributes: Option<&TreeAttributes>,
    path: &[u8],
    mode: u32,
    oid: &ObjectId,
    recurse_submodules: bool,
) -> Result<Option<sley_unpack_trees::StatInfo>> {
    write_tree_entry_to_worktree_with_hooks(
        worktree_root,
        git_dir,
        format,
        db,
        config,
        tree_attributes,
        path,
        mode,
        oid,
        recurse_submodules,
        SubmoduleHooks::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn write_tree_entry_to_worktree_with_hooks(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    tree_attributes: Option<&TreeAttributes>,
    path: &[u8],
    mode: u32,
    oid: &ObjectId,
    recurse_submodules: bool,
    hooks: SubmoduleHooks<'_>,
) -> Result<Option<sley_unpack_trees::StatInfo>> {
    if recurse_submodules
        && sley_index::is_gitlink(mode)
        && gitlink_should_recurse(worktree_root, config, path)
    {
        if let Some(checkout_to_commit) = hooks.checkout_to_commit {
            checkout_to_commit(worktree_root, git_dir, format, path, oid)?;
            return Ok(None);
        }
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

pub fn gitlink_should_recurse(worktree_root: &Path, repo_config: &GitConfig, path: &[u8]) -> bool {
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

/// git's `check_leading_path` reduced to the verify_absent case: walk each
/// leading path component of `git_path` with `lstat`. Return the repo-relative
/// prefix of the first non-directory (symlink or regular file). Return `None`
/// when every leading component is a real directory, or when a component is
/// missing (nothing can be "in the way" through a hole).
///
/// Intermediate symlinks matter because `lstat("frotz/filfre")` follows
/// `frotz` when it is a symlink, making a tracked target leaf look like an
/// untracked file at the new path (t2007 symlink→dir).
fn leading_nondir_component(worktree_root: &Path, git_path: &[u8]) -> Result<Option<Vec<u8>>> {
    let Ok(text) = std::str::from_utf8(git_path) else {
        return Ok(None);
    };
    let mut current = worktree_root.to_path_buf();
    let mut accumulated = Vec::new();
    let mut components = text.split('/').filter(|c| !c.is_empty()).peekable();
    while let Some(component) = components.next() {
        // The leaf itself is checked by the caller via symlink_metadata; only
        // leading components participate in check_leading_path.
        if components.peek().is_none() {
            break;
        }
        if !accumulated.is_empty() {
            accumulated.push(b'/');
        }
        accumulated.extend_from_slice(component.as_bytes());
        current.push(component);
        match fs::symlink_metadata(&current) {
            // Real directory: keep walking.
            Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {}
            // Symlink or regular file (or other non-dir) occupies this component.
            Ok(_) => return Ok(Some(accumulated)),
            Err(err)
                if err.kind() == io::ErrorKind::NotFound
                    || err.kind() == io::ErrorKind::NotADirectory =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(None)
}

/// Emit the porcelain-specific "untracked would be overwritten" abort for
/// `path` and return exit 128, matching git's `add_rejected_path` path for
/// `ERROR_WOULD_LOSE_UNTRACKED_OVERWRITTEN`.
/// Process exit status for an unpack-trees rejection. Upstream dies with 128
/// from the read-tree plumbing, but the checkout/switch porcelain reports the
/// collected "Aborting" block through its normal error return (exit 1).
fn unpack_rejection_exit(porcelain: UnpackPorcelain) -> i32 {
    match porcelain {
        UnpackPorcelain::ReadTree => 128,
        UnpackPorcelain::Checkout => 1,
    }
}

fn reject_untracked_would_be_overwritten(porcelain: UnpackPorcelain, path: &[u8]) -> Result<()> {
    match porcelain {
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
    Err(GitError::Exit(unpack_rejection_exit(porcelain)))
}

/// git's `write_entry` D/F-removal preamble: remove whatever currently occupies
/// `file_path` so a write can proceed. A directory is removed recursively (the
/// dir→file transition, git's `remove_subtree`); a file or symlink is unlinked.
/// An absent path is a no-op.
pub fn remove_path_in_the_way(file_path: &Path) -> Result<()> {
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
pub fn remove_worktree_path(worktree_root: &Path, path: &[u8]) -> Result<()> {
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
pub fn prune_empty_dirs(root: &Path, mut dir: Option<&Path>) {
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

fn original_cwd_relative_to(worktree_root: &Path) -> Option<Vec<u8>> {
    let root = fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    let cwd = original_cwd_absolute()?;
    if cwd == root {
        return None;
    }
    let rel = cwd.strip_prefix(&root).ok()?;
    Some(path_to_git_bytes_lossy(rel))
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

/// Refuse when a planned stage-0 entry would turn the process's original
/// working directory into a regular file (git's CWD D/F guard for the
/// `--reset -u` entry application path).
pub fn refuse_if_unpack_entries_turn_cwd_into_file(
    worktree_root: &Path,
    entries: &[(Vec<u8>, ReadTreeEntry)],
) -> Result<()> {
    let Some(cwd) = original_cwd_relative_to(worktree_root) else {
        return Ok(());
    };
    if entries.iter().any(|(path, entry)| {
        path == &cwd && !sley_index::is_gitlink(entry.mode) && (entry.mode & 0o170000) != 0o040000
    }) && let Some(path) = safe_worktree_path(worktree_root, &cwd)
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
    {
        return refuse_remove_current_working_directory(&cwd);
    }
    Ok(())
}

/// Refuse when applying `result` would delete the directory the process started
/// in and replace it with a regular file (git's "Refusing to remove current
/// working directory" guard for the D/F transition under the CWD).
pub fn refuse_if_unpack_result_removes_current_directory(
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

/// git's `fill_stat_cache_info`/`fill_stat_data`: `lstat` the just-written path
/// and project its fields into the engine's [`sley_unpack_trees::StatInfo`].
/// `size` is git's **munged** on-disk byte length (`munge_st_size` over
/// `metadata.len()`, which sley's `worktree_entry_is_uptodate` compares
/// munged-to-munged), and mtime is the file's real mtime so the racy-clean
/// shortcut can prove the path unchanged.
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
        size: sley_unpack_trees::StatInfo::munge_size(md.len()),
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
        size: sley_unpack_trees::StatInfo::munge_size(md.len()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_submodule::{MoveHeadContext, MoveHeadVerdict, check_submodule_move_head};

    fn config_from(text: &str) -> GitConfig {
        GitConfig::parse(text.as_bytes()).expect("valid config")
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

    #[cfg(unix)]
    #[test]
    fn verify_uptodate_hashes_symlink_target_without_following_directory() {
        let base = std::env::temp_dir().join(format!(
            "sley-rt-symlink-dir-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_dir_all(&base);
        let git_dir = base.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create git directory");
        fs::create_dir_all(base.join("target")).expect("create symlink target directory");
        std::os::unix::fs::symlink("target", base.join("link")).expect("create symlink");
        let oid = EncodedObject::new(ObjectType::Blob, b"target".to_vec())
            .object_id(ObjectFormat::Sha1)
            .expect("hash symlink target");

        verify_uptodate_path(
            &base,
            &git_dir,
            ObjectFormat::Sha1,
            b"link",
            Some(&(sley_index::SYMLINK_MODE, oid)),
            UnpackPorcelain::Checkout,
        )
        .expect("symlink pointing to a directory is clean");

        fs::remove_dir_all(base).expect("clean fixture");
    }

    // ----- end-to-end: active+populated+dirty, non-forced → WouldLose ----

    #[test]
    fn dirty_active_populated_nonforced_would_lose_and_errors() {
        // This is the cell-47/48 shape: a submodule whose HEAD is moving (old
        // set) and whose index is dirty, not forced → ERROR_WOULD_LOSE_SUBMODULE.
        let _set = GitConfig::parse(b"[submodule \"sub1\"]\n\tpath = sub1\n\turl = ./sub1\n")
            .ok();
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
}
