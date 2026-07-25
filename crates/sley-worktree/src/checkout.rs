//! checkout/restore/reset materialization: branch/detached/path checkout, tree materialization, and the symlink-safe blob writer.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::attributes::*;
use crate::filter::*;
use crate::index::*;
use crate::index_io::*;
use crate::status::*;
use crate::types_admin::*;
use sley_pathspec::{PathspecElement, PathspecMatchMagic};

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
        if !worktree_path(worktree_root, entry.path.as_bytes())?.exists()
            && !index_entry_skip_worktree(&entry)
        {
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
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let sparse_checkout_active = sparse_checkout_active_for_status(git_dir, &index);
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        expand_sparse_index(&mut index, &db, format)?;
    }
    // Reuse the same racy-git stat shortcut here: build the cache from the index
    // we just parsed (no second parse) so the worktree walk can skip re-hashing
    // unchanged files. A cached oid is only trusted on a non-racy stat match, so
    // genuinely modified files still fall through to a hash and are reported.
    let stat_cache = IndexStatCache::from_index(&index, &index_path);
    let trust_filemode = trust_executable_bit_from_git_dir(git_dir, None);
    let mut modified = Vec::new();
    for entry in index.entries {
        if index_entry_skip_worktree(&entry) && !sparse_checkout_active {
            continue;
        }
        let worktree_entry = worktree_entry_for_git_path(
            worktree_root,
            git_dir,
            format,
            entry.path.as_bytes(),
            &entry.oid,
            entry.mode,
            Some(&stat_cache),
        )?;
        let Some(worktree_entry) = worktree_entry else {
            if !index_entry_skip_worktree(&entry) {
                modified.push(entry);
            }
            continue;
        };
        let mode_changed = if trust_filemode {
            worktree_entry.mode != entry.mode
        } else {
            sley_diff_merge::is_type_change(entry.mode, worktree_entry.mode)
        };
        if mode_changed || worktree_entry.oid != entry.oid {
            modified.push(entry);
        }
    }
    Ok(modified)
}

/// Compute Git's post-checkout local-change summary against `target_commit`.
///
/// The diff engine treats missing skip-worktree paths as their index entries,
/// so this operation returns the same typed change set for full indexes,
/// sparse checkouts, and sparse indexes.
pub fn checkout_change_summary(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    target_commit: &ObjectId,
) -> Result<CheckoutChangeSummary> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, target_commit)?;
    // checkout's `show_local_changes()` is a raw `diff-index --name-status`
    // report. Unlike user-facing `diff -M`, it does not enable rename
    // detection: a D/F transition that preserves a staged descendant must be
    // rendered as separate D/A rows even when the blobs happen to match.
    let options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: false,
        ..Default::default()
    };
    let changes = sley_diff_merge::diff_name_status_tree_worktree_with_options(
        worktree_root,
        git_dir,
        format,
        &commit.tree,
        options,
    )?;
    Ok(CheckoutChangeSummary { changes })
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
        reapply_active_sparse_checkout(worktree_root, git_dir, format)?;
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
        reapply_active_sparse_checkout(worktree_root, git_dir, format)?;
        0
    } else {
        checkout_commit_to_index_and_worktree_filtered(
            worktree_root,
            git_dir,
            format,
            &target,
            Some(config),
            Some(vec![
                ("ref".to_string(), branch_ref.clone()),
                ("treeish".to_string(), target.to_hex()),
            ]),
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

/// Reconcile an already-current branch with newly enabled or changed sparse
/// checkout rules. A normal same-HEAD checkout remains a no-op; sparse checkout
/// is the exception because `git checkout <current-branch>` is also the legacy
/// command that applies `.git/info/sparse-checkout` to the index and worktree.
/// Reconcile the current index and worktree with the repository's active
/// sparse-checkout definition, if any.
///
/// Tree-producing operations such as a clean three-way merge may construct a
/// temporary full stage-zero index. Calling this after materialization restores
/// skip-worktree bits, removes clean out-of-cone files, and converts the index
/// back to sparse form without advertising a full-index expansion.
pub fn reapply_active_sparse_checkout(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let Some((sparse, mode)) = active_sparse_checkout(git_dir)? else {
        return Ok(());
    };
    apply_sparse_checkout_with_mode(worktree_root, git_dir, format, &sparse, mode)?;
    Ok(())
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
        Some(vec![("treeish".to_string(), target.to_hex())]),
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

pub(crate) fn checkout_commit_to_index_and_worktree(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
) -> Result<usize> {
    checkout_commit_to_index_and_worktree_filtered(
        worktree_root,
        git_dir,
        format,
        target,
        None,
        None,
    )
}

/// Like [`checkout_commit_to_index_and_worktree`] but optionally runs the
/// smudge-side content filters (see [`apply_smudge_filter`]) on each blob before
/// it is written to the worktree. Attribute lookups use the `.gitattributes`
/// recorded in the *target tree* so the rules of the checked-out commit apply.
pub(crate) fn checkout_commit_to_index_and_worktree_filtered(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    smudge_config: Option<&GitConfig>,
    process_metadata: Option<Vec<(String, String)>>,
) -> Result<usize> {
    if let Some((sparse, mode)) = active_sparse_checkout(git_dir)? {
        return checkout_commit_to_index_and_worktree_sparse(
            worktree_root,
            git_dir,
            format,
            target,
            Some((&sparse, mode)),
            smudge_config,
            process_metadata,
        );
    }
    let _process_filter_metadata = set_process_filter_metadata(process_metadata);
    let _process_filter_cwd = set_process_filter_cwd(Some(worktree_root.to_path_buf()));
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    // A branch switch must take the two-way unpack path for either unstaged OR
    // staged changes. Looking only at worktree-vs-index modifications misses a
    // staged path whose worktree already matches the index; the clean rebuild
    // would then discard it, especially across a directory/file transition.
    // The status engine already applies clean filters, so it is also the single
    // correct dirty predicate when smudge configuration is active.
    let mut dirty = checkout_index_has_staged_changes(git_dir, format, &db)?;
    if !dirty {
        stream_short_status(worktree_root, git_dir, format, |entry| {
            if !status_row_is_untracked_or_ignored(entry) {
                dirty = true;
                return Ok(StreamControl::Stop);
            }
            Ok(StreamControl::Continue)
        })?;
    }
    if dirty {
        return Err(GitError::Transaction(
            "checkout requires a clean working tree".into(),
        ));
    }
    let commit = read_commit(&db, format, target)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;
    refuse_if_current_working_directory_becomes_file(worktree_root, &target_entries)?;

    let attributes = smudge_config
        .map(|_| build_tree_attribute_matcher(worktree_root, &db, format, &commit.tree))
        .transpose()?;

    let old_index_entries = read_index_entries(git_dir, format)?;
    for (path, old_entry) in &old_index_entries {
        if !target_entries.contains_key(path) {
            remove_checkout_tracked_path(worktree_root, path, old_entry)?;
        }
    }

    let ignore_case = checkout_should_detect_case_collisions(worktree_root, git_dir);
    let needs_filesystem_collision_probe =
        ignore_case && target_entries.keys().any(|path| !path.is_ascii());
    let collision_probe = needs_filesystem_collision_probe
        .then(|| CheckoutCollisionProbe::new(worktree_root))
        .transpose()?;
    let mut collision_probe = collision_probe;
    let mut materialized_paths = Vec::<CheckoutCollisionPath>::new();
    let mut collided_paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut index_entries = Vec::new();
    let mut prepared_entries = Vec::new();
    let mut delayed_checkout = DelayedCheckoutQueue::default();
    for (path, entry) in &target_entries {
        if ignore_case {
            let folded = checkout_ascii_collision_key(path);
            let filesystem_key = if path.is_ascii() {
                None
            } else {
                collision_probe
                    .as_mut()
                    .map(|probe| probe.key(path))
                    .transpose()?
            };
            if let Some(existing) = materialized_paths.iter().find(|existing| {
                checkout_paths_collide(&existing.ascii, &folded)
                    || existing.filesystem.as_ref().is_some_and(|existing_key| {
                        filesystem_key
                            .as_ref()
                            .is_some_and(|key| checkout_filesystem_paths_collide(existing_key, key))
                    })
            }) {
                collided_paths.insert(existing.original.clone());
                collided_paths.insert(path.clone());
                index_entries.push(unmaterialized_index_entry(path, entry));
                continue;
            }
            materialized_paths.push(CheckoutCollisionPath {
                ascii: folded,
                filesystem: filesystem_key,
                original: path.clone(),
            });
        }
        match prepare_checkout_entry(
            &db,
            format,
            path,
            entry,
            smudge_config,
            attributes.as_ref(),
            &mut delayed_checkout,
        )? {
            PreparedCheckoutResult::Ready(prepared) => prepared_entries.push(prepared),
            PreparedCheckoutResult::Delayed(entry) => index_entries.push(entry),
        }
    }
    drop(collision_probe);
    let default_config = GitConfig::default();
    index_entries.extend(materialize_prepared_checkout_entries(
        worktree_root,
        smudge_config.unwrap_or(&default_config),
        prepared_entries,
    )?);
    let mut delayed_updates = finish_delayed_checkout(worktree_root, delayed_checkout)?;
    for entry in &mut index_entries {
        if let Some(updated) = delayed_updates.remove(entry.path.as_bytes()) {
            *entry = updated;
        }
    }
    warn_checkout_collisions(&collided_paths);
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let extensions = preserved_index_extensions(git_dir, format)?;
    let mut index = Index {
        version: 2,
        entries: index_entries,
        extensions,
        checksum: None,
    };
    refresh_cache_tree(&mut index, &db);
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(target_entries.len())
}

fn remove_checkout_tracked_path(
    worktree_root: &Path,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<()> {
    if !sley_index::is_gitlink(entry.mode) {
        return remove_worktree_file(worktree_root, path);
    }
    let file = worktree_path(worktree_root, path)?;
    if !file.exists() {
        return Ok(());
    }
    if !file.is_dir() {
        return remove_worktree_file(worktree_root, path);
    }
    match fs::remove_dir(&file) {
        Ok(()) => prune_empty_parents(worktree_root, file.parent())?,
        Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            eprintln!(
                "warning: unable to rmdir '{}': Directory not empty",
                String::from_utf8_lossy(path)
            );
        }
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

fn checkout_should_detect_case_collisions(worktree_root: &Path, git_dir: &Path) -> bool {
    GitConfig::read(git_dir.join("config"))
        .ok()
        .and_then(|config| config.get_bool("core", None, "ignorecase"))
        .unwrap_or(false)
        || filesystem_is_case_insensitive(worktree_root)
}

fn filesystem_is_case_insensitive(root: &Path) -> bool {
    let probe = root.join(format!(".sley-case-probe-{}", std::process::id()));
    let upper = root.join(format!(".SLEY-CASE-PROBE-{}", std::process::id()));
    let result = (|| -> std::io::Result<bool> {
        fs::write(&probe, b"lower")?;
        Ok(upper.exists())
    })();
    let _ = fs::remove_file(&probe);
    if upper != probe {
        let _ = fs::remove_file(&upper);
    }
    result.unwrap_or(false)
}

struct CheckoutCollisionPath {
    ascii: Vec<u8>,
    filesystem: Option<Vec<u64>>,
    original: Vec<u8>,
}

struct CheckoutCollisionProbe {
    root: PathBuf,
    keys: BTreeMap<PathBuf, Vec<u64>>,
}

impl CheckoutCollisionProbe {
    fn new(worktree_root: &Path) -> Result<Self> {
        static NEXT_PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let serial = NEXT_PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = worktree_root.join(format!(
                ".sley-checkout-collision-{}-{serial}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    return Ok(Self {
                        root,
                        keys: BTreeMap::new(),
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn key(&mut self, path: &[u8]) -> Result<Vec<u64>> {
        use std::hash::{Hash, Hasher};

        let relative = repo_path_to_os_path(path)?;
        let mut current = self.root.clone();
        let mut relative_prefix = PathBuf::new();
        let mut key = Vec::new();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(GitError::InvalidPath(format!(
                    "invalid checkout collision path {}",
                    String::from_utf8_lossy(path)
                )));
            };
            current.push(name);
            relative_prefix.push(name);
            if let Some(cached) = self.keys.get(&relative_prefix) {
                key.clone_from(cached);
                continue;
            }
            match fs::create_dir(&current) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
            let handle = same_file::Handle::from_path(&current)?;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            handle.hash(&mut hasher);
            key.push(hasher.finish());
            self.keys.insert(relative_prefix.clone(), key.clone());
        }
        Ok(key)
    }
}

impl Drop for CheckoutCollisionProbe {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn checkout_ascii_collision_key(path: &[u8]) -> Vec<u8> {
    path.iter().map(u8::to_ascii_lowercase).collect()
}

fn checkout_filesystem_paths_collide(left: &[u64], right: &[u64]) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn checkout_paths_collide(left: &[u8], right: &[u8]) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with(b"/"))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with(b"/"))
}

fn unmaterialized_index_entry(path: &[u8], entry: &TrackedEntry) -> IndexEntry {
    IndexEntry {
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
        flags: path.len().min(0x0fff) as u16,
        flags_extended: 0,
        path: BString::from(path),
    }
}

pub(crate) fn unmaterialized_index_entry_from_index(entry: &IndexEntry) -> IndexEntry {
    let mut unmaterialized = entry.clone();
    unmaterialized.ctime_seconds = 0;
    unmaterialized.ctime_nanoseconds = 0;
    unmaterialized.mtime_seconds = 0;
    unmaterialized.mtime_nanoseconds = 0;
    unmaterialized.dev = 0;
    unmaterialized.ino = 0;
    unmaterialized.uid = 0;
    unmaterialized.gid = 0;
    unmaterialized.size = 0;
    unmaterialized
}

#[derive(Default)]
pub(crate) struct DelayedCheckoutQueue {
    filters: BTreeSet<String>,
    pending: BTreeMap<Vec<u8>, DelayedCheckoutEntry>,
}

struct DelayedCheckoutEntry {
    process: String,
    entry: TrackedEntry,
}

struct PreparedCheckoutEntry {
    path: Vec<u8>,
    entry: TrackedEntry,
    body: Option<Vec<u8>>,
    index_template: Option<IndexEntry>,
}

enum PreparedCheckoutResult {
    Ready(PreparedCheckoutEntry),
    Delayed(IndexEntry),
}

fn prepare_checkout_entry(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    path: &[u8],
    entry: &TrackedEntry,
    smudge_config: Option<&GitConfig>,
    attributes: Option<&AttributeMatcher>,
    delayed: &mut DelayedCheckoutQueue,
) -> Result<PreparedCheckoutResult> {
    if sley_index::is_gitlink(entry.mode) {
        return Ok(PreparedCheckoutResult::Ready(PreparedCheckoutEntry {
            path: path.to_vec(),
            entry: entry.clone(),
            body: None,
            index_template: None,
        }));
    }
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    let body = if (entry.mode & 0o170000) == 0o120000 {
        object.body.clone()
    } else if let (Some(config), Some(matcher)) = (smudge_config, attributes) {
        let checks = matcher.attributes_for_path(path, &filter_attribute_names(), false);
        match apply_smudge_filter_with_attributes_maybe_delayed(
            config,
            &checks,
            path,
            &object.body,
            format,
            true,
        )? {
            SmudgeFilterResult::Content(body) => body.into_owned(),
            SmudgeFilterResult::Delayed { process } => {
                delayed.enqueue(process, path, entry);
                return Ok(PreparedCheckoutResult::Delayed(unmaterialized_index_entry(
                    path, entry,
                )));
            }
        }
    } else {
        object.body.clone()
    };
    Ok(PreparedCheckoutResult::Ready(PreparedCheckoutEntry {
        path: path.to_vec(),
        entry: entry.clone(),
        body: Some(body),
        index_template: None,
    }))
}

fn prepare_index_checkout_entry(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    entry: &IndexEntry,
    smudge_config: Option<&GitConfig>,
    stat_cache: Option<&IndexStatCache>,
    delayed: &mut DelayedCheckoutQueue,
) -> Result<Option<PreparedCheckoutResult>> {
    if sley_index::is_gitlink(entry.mode) {
        let dir_path = worktree_path(worktree_root, entry.path.as_bytes())?;
        materialize_gitlink_dir(worktree_root, &dir_path)?;
        return Ok(None);
    }
    let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
    if let Some(stat_cache) = stat_cache
        && let Ok(metadata) = fs::symlink_metadata(&file_path)
    {
        if stat_cache
            .reuse_index_entry_for_checkout(entry, &metadata)
            .is_some()
        {
            return Ok(None);
        }
        if stat_cache.is_racy_checkout_stat_match(entry, &metadata)
            && worktree_entry_for_git_path(
                worktree_root,
                git_dir,
                format,
                entry.path.as_bytes(),
                &entry.oid,
                entry.mode,
                Some(stat_cache),
            )?
            .is_some_and(|worktree_entry| {
                worktree_entry.mode == entry.mode && worktree_entry.oid == entry.oid
            })
        {
            return Ok(None);
        }
    }
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    let body = match smudge_config {
        Some(config) if (entry.mode & 0o170000) != 0o120000 => {
            let checks = smudge_attribute_checks_from_index(
                worktree_root,
                git_dir,
                format,
                entry.path.as_bytes(),
            )?;
            match apply_smudge_filter_with_attributes_maybe_delayed(
                config,
                &checks,
                entry.path.as_bytes(),
                &object.body,
                format,
                true,
            )? {
                SmudgeFilterResult::Content(body) => body.into_owned(),
                SmudgeFilterResult::Delayed { process } => {
                    delayed.enqueue(
                        process,
                        entry.path.as_bytes(),
                        &TrackedEntry {
                            mode: entry.mode,
                            oid: entry.oid,
                        },
                    );
                    return Ok(Some(PreparedCheckoutResult::Delayed(
                        unmaterialized_index_entry_from_index(entry),
                    )));
                }
            }
        }
        _ => object.body.clone(),
    };
    Ok(Some(PreparedCheckoutResult::Ready(PreparedCheckoutEntry {
        path: entry.path.as_bytes().to_vec(),
        entry: TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        },
        body: Some(body),
        index_template: Some(entry.clone()),
    })))
}

fn materialize_prepared_checkout_entry(
    worktree_root: &Path,
    prepared: PreparedCheckoutEntry,
) -> Result<IndexEntry> {
    let PreparedCheckoutEntry {
        path,
        entry,
        body,
        index_template,
    } = prepared;
    if sley_index::is_gitlink(entry.mode) {
        let dir_path = worktree_path(worktree_root, &path)?;
        materialize_gitlink_dir(worktree_root, &dir_path)?;
        return Ok(index_template.unwrap_or_else(|| unmaterialized_index_entry(&path, &entry)));
    }
    let body = body.ok_or_else(|| {
        GitError::InvalidFormat("checkout blob materialization had no body".into())
    })?;
    let file_path = worktree_path(worktree_root, &path)?;
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    remove_existing_worktree_path(&file_path)?;
    write_blob_body_or_symlink(&file_path, entry.mode, &body, &body)?;
    let metadata = fs::symlink_metadata(&file_path)?;
    let mut index_entry = match index_template {
        Some(template) => index_entry_with_refreshed_stat(&template, &metadata),
        None => index_entry_from_metadata(path, entry.oid, &metadata),
    };
    index_entry.mode = entry.mode;
    Ok(index_entry)
}

fn checkout_worker_collision_key(path: &[u8]) -> Vec<u8> {
    path.split(|byte| *byte == b'/')
        .next()
        .unwrap_or(path)
        .to_vec()
}

fn materialize_prepared_checkout_entries(
    worktree_root: &Path,
    config: &GitConfig,
    prepared: Vec<PreparedCheckoutEntry>,
) -> Result<Vec<IndexEntry>> {
    let plan = ParallelCheckoutPlan::from_config(config, prepared.len());
    if plan.worker_count == 0 {
        return prepared
            .into_iter()
            .map(|entry| materialize_prepared_checkout_entry(worktree_root, entry))
            .collect();
    }

    let mut lock_by_prefix = BTreeMap::new();
    for entry in &prepared {
        lock_by_prefix
            .entry(checkout_worker_collision_key(&entry.path))
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())));
    }
    let queue = std::sync::Arc::new(std::sync::Mutex::new(
        prepared
            .into_iter()
            .enumerate()
            .collect::<std::collections::VecDeque<_>>(),
    ));
    let results = std::sync::Arc::new(std::sync::Mutex::new(
        (0..plan.item_count)
            .map(|_| None)
            .collect::<Vec<Option<Result<IndexEntry>>>>(),
    ));
    let locks = std::sync::Arc::new(lock_by_prefix);
    let worker_argv = vec!["git".to_string(), "checkout--worker".to_string()];

    std::thread::scope(|scope| {
        for worker_id in 0..plan.worker_count {
            sley_core::trace2::child_start_with_id("checkout", worker_id, &worker_argv);
            let queue = std::sync::Arc::clone(&queue);
            let results = std::sync::Arc::clone(&results);
            let locks = std::sync::Arc::clone(&locks);
            scope.spawn(move || {
                loop {
                    let next = queue
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .pop_front();
                    let Some((position, entry)) = next else {
                        break;
                    };
                    let prefix = checkout_worker_collision_key(&entry.path);
                    let result = if let Some(path_lock) = locks.get(&prefix) {
                        let _guard = path_lock
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        materialize_prepared_checkout_entry(worktree_root, entry)
                    } else {
                        materialize_prepared_checkout_entry(worktree_root, entry)
                    };
                    results
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())[position] = Some(result);
                }
            });
        }
    });

    let mut results = results
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    results
        .drain(..)
        .map(|result| {
            result.unwrap_or_else(|| {
                Err(GitError::Transaction(
                    "parallel checkout worker did not report a result".into(),
                ))
            })
        })
        .collect()
}

/// Materialize an unpack-trees checkout batch through the shared worker queue.
///
/// Content conversion is resolved before workers start, so external filters
/// remain single-session/sequential while independent filesystem writes run in
/// parallel. Recursive gitlinks deliberately stay with the caller.
pub fn materialize_checkout_entries_with_database(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    attributes: Option<&TreeAttributes>,
    entries: &[CheckoutMaterializationEntry],
) -> Result<CheckoutMaterializationOutcome> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let mut prepared = Vec::with_capacity(entries.len());
    for entry in entries {
        if sley_index::is_gitlink(entry.mode) {
            return Err(GitError::InvalidFormat(
                "recursive gitlink passed to blob checkout materializer".into(),
            ));
        }
        let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
        let body = if (entry.mode & 0o170000) == 0o120000 {
            object.body.clone()
        } else {
            match attributes {
                Some(attributes) => {
                    attributes.apply_smudge_filter(config, &entry.path, &object.body)?
                }
                None => apply_smudge_filter(
                    worktree_root,
                    git_dir,
                    format,
                    config,
                    &entry.path,
                    &object.body,
                )?,
            }
        };
        prepared.push(PreparedCheckoutEntry {
            path: entry.path.clone(),
            entry: TrackedEntry {
                mode: entry.mode,
                oid: entry.oid,
            },
            body: Some(body),
            index_template: None,
        });
    }
    let materialized = materialize_prepared_checkout_entries(worktree_root, config, prepared)?;
    let stats = materialized
        .into_iter()
        .map(|entry| {
            let stat = sley_unpack_trees::StatInfo {
                ctime_seconds: entry.ctime_seconds,
                ctime_nanoseconds: entry.ctime_nanoseconds,
                mtime_seconds: entry.mtime_seconds,
                mtime_nanoseconds: entry.mtime_nanoseconds,
                dev: entry.dev,
                ino: entry.ino,
                uid: entry.uid,
                gid: entry.gid,
                size: entry.size,
            };
            (entry.path.into_bytes(), Some(stat))
        })
        .collect();
    Ok(CheckoutMaterializationOutcome { stats })
}

impl DelayedCheckoutQueue {
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn enqueue(&mut self, process: String, path: &[u8], entry: &TrackedEntry) {
        self.filters.insert(process.clone());
        self.pending.insert(
            path.to_vec(),
            DelayedCheckoutEntry {
                process,
                entry: entry.clone(),
            },
        );
    }
}

pub(crate) fn finish_delayed_checkout(
    worktree_root: &Path,
    delayed: DelayedCheckoutQueue,
) -> Result<BTreeMap<Vec<u8>, IndexEntry>> {
    let outcome = finish_delayed_checkout_outcome(worktree_root, delayed)?;
    if outcome.had_error {
        return Err(GitError::Exit(1));
    }
    Ok(outcome.updates)
}

struct DelayedCheckoutFinishOutcome {
    updates: BTreeMap<Vec<u8>, IndexEntry>,
    failed_paths: Vec<Vec<u8>>,
    had_error: bool,
}

fn finish_delayed_checkout_outcome(
    worktree_root: &Path,
    mut delayed: DelayedCheckoutQueue,
) -> Result<DelayedCheckoutFinishOutcome> {
    if delayed.is_empty() {
        return Ok(DelayedCheckoutFinishOutcome {
            updates: BTreeMap::new(),
            failed_paths: Vec::new(),
            had_error: false,
        });
    }

    let mut updates = BTreeMap::new();
    let mut failed_paths = BTreeSet::new();
    let mut had_error = false;
    let mut active_filters = delayed.filters.iter().cloned().collect::<Vec<_>>();
    while !active_filters.is_empty() {
        let mut next_filters = Vec::new();
        for process in active_filters {
            let mut available = match list_available_process_filter_blobs(&process) {
                Ok(paths) => paths,
                Err(err) => {
                    if err.protocol {
                        eprintln!("error: external filter '{}' failed", process);
                    }
                    had_error = true;
                    continue;
                }
            };
            if available.is_empty() {
                continue;
            }
            available.sort();
            available.dedup();

            let mut keep_filter = true;
            for path in available {
                let Some(delayed_entry) = delayed.pending.remove(path.as_slice()) else {
                    eprintln!(
                        "error: external filter '{}' signaled that '{}' is now available although it has not been delayed earlier",
                        process,
                        String::from_utf8_lossy(&path)
                    );
                    had_error = true;
                    failed_paths.insert(path);
                    keep_filter = false;
                    continue;
                };
                if delayed_entry.process != process {
                    eprintln!(
                        "error: external filter '{}' signaled that '{}' is now available although it has not been delayed earlier",
                        process,
                        String::from_utf8_lossy(&path)
                    );
                    had_error = true;
                    failed_paths.insert(path);
                    keep_filter = false;
                    continue;
                }

                match run_process_filter(
                    &process,
                    "smudge",
                    &path,
                    &[],
                    Some(delayed_entry.entry.oid),
                    false,
                ) {
                    Ok(ProcessFilterOutcome::Filtered(output)) => {
                        match write_delayed_checkout_output(
                            worktree_root,
                            &path,
                            &delayed_entry.entry,
                            &output,
                        ) {
                            Ok(Some(index_entry)) => {
                                updates.insert(path, index_entry);
                            }
                            Ok(None) => {
                                failed_paths.insert(path);
                                had_error = true;
                            }
                            Err(_) => {
                                failed_paths.insert(path);
                                had_error = true;
                            }
                        }
                    }
                    Ok(ProcessFilterOutcome::Unsupported) => {
                        eprintln!("error: external filter '{}' failed", process);
                        had_error = true;
                        failed_paths.insert(path);
                        keep_filter = false;
                    }
                    Ok(ProcessFilterOutcome::Status(status)) => {
                        eprintln!(
                            "error: external filter '{}' returned status {status}",
                            process
                        );
                        had_error = true;
                        failed_paths.insert(path);
                        keep_filter = false;
                    }
                    Err(err) => {
                        if err.protocol {
                            eprintln!("error: external filter '{}' failed", process);
                        }
                        had_error = true;
                        failed_paths.insert(path);
                        keep_filter = false;
                    }
                }
            }

            if keep_filter {
                next_filters.push(process);
            }
        }
        active_filters = next_filters;
    }

    for path in delayed.pending.keys() {
        eprintln!(
            "error: '{}' was not filtered properly",
            String::from_utf8_lossy(path)
        );
        had_error = true;
        failed_paths.insert(path.clone());
    }

    Ok(DelayedCheckoutFinishOutcome {
        updates,
        failed_paths: failed_paths.into_iter().collect(),
        had_error,
    })
}

fn write_delayed_checkout_output(
    worktree_root: &Path,
    path: &[u8],
    entry: &TrackedEntry,
    body: &[u8],
) -> Result<Option<IndexEntry>> {
    if checkout_path_has_symlink_parent(worktree_root, path)? {
        return Ok(None);
    }
    let file_path = worktree_path(worktree_root, path)?;
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    remove_existing_worktree_path(&file_path)?;
    fs::write(&file_path, body)?;
    set_worktree_file_mode(&file_path, entry.mode)?;
    let metadata = fs::metadata(&file_path)?;
    let mut index_entry = index_entry_from_metadata(path.to_vec(), entry.oid, &metadata);
    index_entry.mode = entry.mode;
    Ok(Some(index_entry))
}

fn checkout_path_has_symlink_parent(worktree_root: &Path, path: &[u8]) -> Result<bool> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let mut current = worktree_root.to_path_buf();
    let mut components = Path::new(rel).components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        }
    }
    Ok(false)
}

fn warn_checkout_collisions(collided_paths: &BTreeSet<Vec<u8>>) {
    if collided_paths.is_empty() {
        return;
    }
    eprintln!("warning: the following paths have collided:");
    for path in collided_paths {
        eprintln!("{}", String::from_utf8_lossy(path));
    }
}

/// Build an [`AttributeMatcher`] from the `.gitattributes` files contained in a
/// tree, plus the repo-level (`core.attributesFile`, `.git/info/attributes`)
/// sources, mirroring [`standard_attributes_for_path_from_tree`].
pub(crate) fn build_tree_attribute_matcher(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<AttributeMatcher> {
    let mut matcher = AttributeMatcher::default();
    let git_dir = worktree_root.join(".git");
    matcher.configure_case_sensitivity(&git_dir);
    if !matcher.read_configured_attributes(worktree_root, &git_dir) {
        matcher.read_default_global_attributes();
    }
    collect_attribute_patterns_from_tree(db, format, tree_oid, Vec::new(), &mut matcher)?;
    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
        false,
    );
    Ok(matcher)
}

pub(crate) fn materialize_tree_entry_with_optional_smudge(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    worktree_root: &Path,
    path: &[u8],
    entry: &TrackedEntry,
    smudge_config: Option<&GitConfig>,
    attributes: Option<&AttributeMatcher>,
    delayed: Option<&mut DelayedCheckoutQueue>,
) -> Result<IndexEntry> {
    // A symlink (mode 120000) is written as a *symlink* whose target is the raw,
    // unfiltered blob bytes — git treats symlink content as an opaque path, so no
    // smudge/EOL filter ever applies. Route it through the type-aware
    // `materialize_tree_entry` (→ `write_worktree_blob_entry`) so it is never
    // materialized as a regular file holding the target string. A gitlink (mkdir,
    // no blob read) and the no-smudge case go through the same shared path.
    if smudge_config.is_none()
        || sley_index::is_gitlink(entry.mode)
        || (entry.mode & 0o170000) == 0o120000
    {
        return materialize_tree_entry(db, worktree_root, path, entry);
    }
    let Some(config) = smudge_config else {
        return materialize_tree_entry(db, worktree_root, path, entry);
    };
    let Some(matcher) = attributes else {
        return materialize_tree_entry(db, worktree_root, path, entry);
    };
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    let checks = matcher.attributes_for_path(path, &filter_attribute_names(), false);
    let body = match apply_smudge_filter_with_attributes_maybe_delayed(
        config,
        &checks,
        path,
        &object.body,
        format,
        delayed.is_some(),
    )? {
        SmudgeFilterResult::Content(body) => body,
        SmudgeFilterResult::Delayed { process } => {
            if let Some(queue) = delayed {
                queue.enqueue(process, path, entry);
                return Ok(unmaterialized_index_entry(path, entry));
            }
            return Err(GitError::InvalidFormat(
                "smudge filter requested delay without a checkout queue".into(),
            ));
        }
    };
    let file_path = worktree_path(worktree_root, path)?;
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    remove_existing_worktree_path(&file_path)?;
    fs::write(&file_path, &body)?;
    set_worktree_file_mode(&file_path, entry.mode)?;
    let metadata = fs::metadata(&file_path)?;
    let mut index_entry = index_entry_from_metadata(path.to_vec(), entry.oid, &metadata);
    index_entry.mode = entry.mode;
    Ok(index_entry)
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
/// Sparse-aware checkout of a commit: only materializes in-cone paths; out-of-cone
/// index entries get skip-worktree and no worktree file (blobs may be absent).
pub fn checkout_commit_to_index_and_worktree_sparse(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    sparse: Option<(&SparseCheckout, SparseCheckoutMode)>,
    smudge_config: Option<&GitConfig>,
    process_metadata: Option<Vec<(String, String)>>,
) -> Result<usize> {
    let _process_filter_metadata = set_process_filter_metadata(process_metadata);
    let _process_filter_cwd = set_process_filter_cwd(Some(worktree_root.to_path_buf()));
    let previously_skipped = skip_worktree_paths(git_dir, format)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, target)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;

    // Honor skip-worktree: a path whose worktree file is intentionally absent
    // must not be treated as a dirty (deleted) change blocking the checkout.
    // A staged change on such a path still matters and must force the shared
    // two-way transition instead of being discarded by a clean rebuild.
    let mut dirty = checkout_index_has_staged_changes(git_dir, format, &db)?;
    if !dirty {
        stream_short_status(worktree_root, git_dir, format, |entry| {
            if previously_skipped.contains(entry.path) && entry.index == b' ' {
                return Ok(StreamControl::Continue);
            }
            // Submodule state never blocks a checkout: upstream unpack-trees
            // treats gitlinks as always up-to-date (ie_match_stat refuses to pay
            // for a submodule dirtiness probe), so new commits / dirty content in
            // a submodule must not fail the branch switch.
            if entry.index_mode.is_some_and(sley_index::is_gitlink)
                || entry.worktree_mode.is_some_and(sley_index::is_gitlink)
            {
                return Ok(StreamControl::Continue);
            }
            // An untracked embedded repository where the target tree records a
            // gitlink is reused as-is (upstream entry.c write_entry: mkdir with
            // EEXIST is success), so it does not block the checkout either.
            if entry.index == b'?' && entry.worktree == b'?' {
                let path = entry.path.strip_suffix(b"/").unwrap_or(entry.path);
                if target_entries
                    .get(path)
                    .is_some_and(|target| sley_index::is_gitlink(target.mode))
                {
                    return Ok(StreamControl::Continue);
                }
            }
            dirty = true;
            Ok(StreamControl::Stop)
        })?;
    }
    if dirty {
        return Err(GitError::Transaction(
            "checkout requires a clean working tree".into(),
        ));
    }

    let matcher = sparse.map(|(spec, mode)| SparseMatcher::new(spec, mode));
    let attributes = smudge_config
        .map(|_| build_tree_attribute_matcher(worktree_root, &db, format, &commit.tree))
        .transpose()?;

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
    let mut delayed_checkout = DelayedCheckoutQueue::default();
    for (path, entry) in &target_entries {
        let in_cone = matcher.as_ref().map_or_else(
            || !previously_skipped.contains(path),
            |matcher| matcher.includes_file(path),
        );
        let index_entry = if in_cone {
            materialize_tree_entry_with_optional_smudge(
                &db,
                format,
                worktree_root,
                path,
                entry,
                smudge_config,
                attributes.as_ref(),
                Some(&mut delayed_checkout),
            )?
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
    let mut delayed_updates = finish_delayed_checkout(worktree_root, delayed_checkout)?;
    for entry in &mut index_entries {
        if let Some(updated) = delayed_updates.remove(entry.path.as_bytes()) {
            *entry = updated;
        }
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut index = Index {
        version: 2,
        entries: index_entries,
        extensions: preserved_index_extensions(git_dir, format)?,
        checksum: None,
    };
    normalize_index_version_for_extended_flags(&mut index);
    refresh_cache_tree(&mut index, &db);
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(target_entries.len())
}

pub(crate) fn skip_worktree_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeSet<Vec<u8>>> {
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

/// Whether the stage-zero index differs from HEAD, using the actual entry table
/// instead of CACHE_TREE. A command that staged paths may leave that extension
/// stale until the index is rewritten; trusting it here can misclassify a dirty
/// checkout as clean and discard staged changes during a branch switch.
fn checkout_index_has_staged_changes(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<bool> {
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(index_path)?, format)?
    } else {
        return Ok(resolve_head_tree_oid(git_dir, format, db)?.is_some());
    };
    if index.is_sparse() {
        for entry in &mut index.entries {
            if entry.mode == sley_index::SPARSE_DIR_MODE && entry.path.as_bytes().ends_with(b"/") {
                entry.set_skip_worktree(true);
            }
        }
        expand_sparse_index_view(&mut index, db, format)?;
    }
    let index_entries = index
        .entries
        .iter()
        .filter(|entry| entry.stage() == sley_index::Stage::Normal)
        .map(|entry| (entry.path.as_bytes().to_vec(), (entry.mode, entry.oid)))
        .collect::<BTreeMap<_, _>>();
    let head_entries = head_tree_entries(git_dir, format, db)?
        .into_iter()
        .map(|(path, entry)| (path, (entry.mode, entry.oid)))
        .collect::<BTreeMap<_, _>>();
    Ok(index_entries != head_entries)
}

pub fn restore_worktree_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    restore_worktree_paths_inner(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        paths,
        None,
    )
}

/// Like [`restore_worktree_paths`], applying the smudge-side content filters
/// (CRLF / ident / filter drivers) the way a checkout writes blobs.
pub fn restore_worktree_paths_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    config: &GitConfig,
) -> Result<RestoreResult> {
    restore_worktree_paths_inner(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        paths,
        Some(config),
    )
}

pub(crate) fn restore_worktree_paths_inner(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    smudge_config: Option<&GitConfig>,
) -> Result<RestoreResult> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Err(GitError::Exit(1));
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let stat_cache = IndexStatCache::from_index(&index, &index_path);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut restored = BTreeSet::new();
    let selected = checkout_selected_positions(
        worktree_root,
        paths,
        index
            .entries
            .iter()
            .enumerate()
            .map(|(position, entry)| (position, entry.path.as_bytes())),
        false,
    )?;
    for position in selected {
        let refreshed = restore_index_entry(
            worktree_root,
            git_dir,
            format,
            &db,
            &index.entries[position],
            smudge_config,
            Some(&stat_cache),
        )?;
        restored.insert(index.entries[position].path.clone());
        if let Some(refreshed) = refreshed {
            index.entries[position] = refreshed;
        }
    }
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn checkout_index_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: CheckoutIndexPathOptions<'_>,
) -> Result<RestoreResult> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    checkout_index_paths_with_database(worktree_root, git_dir, format, paths, &db, options)
}

/// Restore index paths using an explicitly configured object database.
///
/// Embedders use this variant when object content reads carry repository
/// policy such as replacement refs. Index parsing/writing remains keyed by
/// the raw object ids stored in the index; only blob content reads use `db`.
pub fn checkout_index_paths_with_database(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    db: &FileObjectDatabase,
    options: CheckoutIndexPathOptions<'_>,
) -> Result<RestoreResult> {
    let outcome = checkout_index_paths_with_database_outcome(
        worktree_root,
        git_dir,
        format,
        paths,
        db,
        options,
    )?;
    if let Some(failure) = outcome.failures.into_iter().next() {
        return Err(failure.error);
    }
    Ok(RestoreResult {
        restored: outcome.restored,
    })
}

/// Restore all independently viable paths and retain per-path failures.
///
/// This is the path-checkout equivalent of Git's parallel-checkout result
/// collection: one missing object does not prevent unrelated entries or
/// delayed filters from finishing. Callers still decide how to render the
/// successful count and which retained error determines their exit status.
pub fn checkout_index_paths_with_database_outcome(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    db: &FileObjectDatabase,
    options: CheckoutIndexPathOptions<'_>,
) -> Result<CheckoutIndexPathOutcome> {
    checkout_index_paths_with_database_outcome_sparse(
        worktree_root,
        git_dir,
        format,
        paths,
        db,
        CheckoutIndexSparsePolicy::Ignore,
        options,
    )
}

/// Restore index paths with an explicit sparse-selection policy.
///
/// The historical embedding API above retains its all-index behavior. Git
/// porcelain should use this operation with [`CheckoutIndexSparsePolicy::Honor`]
/// by default and switch to `Ignore` only for its explicit override.
pub fn checkout_index_paths_with_database_outcome_sparse(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    db: &FileObjectDatabase,
    sparse_policy: CheckoutIndexSparsePolicy,
    options: CheckoutIndexPathOptions<'_>,
) -> Result<CheckoutIndexPathOutcome> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Err(GitError::Exit(1));
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    if options.merge {
        checkout_unmerge_resolve_undo_paths(worktree_root, &mut index, format, paths)?;
    }
    let stat_cache = IndexStatCache::from_index(&index, &index_path);
    let selected = checkout_selected_index_paths(worktree_root, &index, paths)?;

    if options.stage.is_none() && !options.merge && !options.force {
        for path in &selected {
            if checkout_path_is_unmerged(&index, path) {
                eprintln!(
                    "error: path '{}' is unmerged",
                    String::from_utf8_lossy(path)
                );
                return Err(GitError::Exit(1));
            }
        }
    }

    let mut refreshed = BTreeMap::new();
    let mut restored = BTreeSet::new();
    let mut prepared_entries = Vec::new();
    let mut prepared_positions = BTreeMap::new();
    let mut failures = Vec::new();
    let mut delayed_checkout = DelayedCheckoutQueue::default();
    for path in selected {
        let positions = index
            .entries
            .iter()
            .enumerate()
            .filter_map(|(position, entry)| (entry.path.as_bytes() == path).then_some(position))
            .collect::<Vec<_>>();
        let stage0 = positions
            .iter()
            .copied()
            .find(|position| index.entries[*position].stage() == Stage::Normal);
        let is_unmerged = positions
            .iter()
            .any(|position| index.entries[*position].stage() != Stage::Normal);

        if sparse_policy == CheckoutIndexSparsePolicy::Honor
            && stage0.is_some_and(|position| index.entries[position].is_skip_worktree())
            && !is_unmerged
        {
            continue;
        }

        if is_unmerged {
            if let Some(stage) = options.stage {
                let wanted = match stage {
                    CheckoutStage::Ours => Stage::Ours,
                    CheckoutStage::Theirs => Stage::Theirs,
                };
                let Some(position) = positions
                    .iter()
                    .copied()
                    .find(|position| index.entries[*position].stage() == wanted)
                else {
                    if !options.overlay {
                        remove_worktree_file(worktree_root, &path)?;
                        restored.insert(path);
                        continue;
                    }
                    eprintln!(
                        "error: path '{}' does not have {} version",
                        String::from_utf8_lossy(&path),
                        match stage {
                            CheckoutStage::Ours => "our",
                            CheckoutStage::Theirs => "their",
                        }
                    );
                    return Err(GitError::Exit(1));
                };
                if restore_index_entry_maybe_delayed(
                    worktree_root,
                    git_dir,
                    format,
                    db,
                    &index.entries[position],
                    options.smudge_config,
                    Some(&stat_cache),
                    Some(&mut delayed_checkout),
                )?
                .is_some()
                {
                    restored.insert(path);
                }
                continue;
            }
            if options.merge {
                checkout_merge_unmerged_path(
                    worktree_root,
                    db,
                    &index,
                    &positions,
                    options.conflict_style,
                )?;
                restored.insert(path);
                continue;
            }
            if options.force {
                continue;
            }
        }

        if let Some(position) = stage0 {
            let mut checkout_entry = index.entries[position].clone();
            if sparse_policy == CheckoutIndexSparsePolicy::Ignore {
                clear_skip_worktree(&mut checkout_entry);
            }
            let prepared = prepare_index_checkout_entry(
                worktree_root,
                git_dir,
                format,
                db,
                &checkout_entry,
                options.smudge_config,
                Some(&stat_cache),
                &mut delayed_checkout,
            );
            match prepared {
                Ok(Some(PreparedCheckoutResult::Ready(prepared))) => {
                    prepared_positions.insert(prepared.path.clone(), position);
                    prepared_entries.push(prepared);
                    restored.insert(path);
                }
                Ok(Some(PreparedCheckoutResult::Delayed(entry))) => {
                    refreshed.insert(position, entry);
                }
                Ok(None) => {}
                Err(error) => failures.push(CheckoutPathFailure { path, error }),
            }
        }
    }

    let default_config = GitConfig::default();
    let materialized = materialize_prepared_checkout_entries(
        worktree_root,
        options.smudge_config.unwrap_or(&default_config),
        prepared_entries,
    )?;
    for entry in materialized {
        if let Some(position) = prepared_positions.get(entry.path.as_bytes()).copied() {
            refreshed.insert(position, entry);
        }
    }

    let mut delayed_finish = finish_delayed_checkout_outcome(worktree_root, delayed_checkout)?;
    for path in delayed_finish.failed_paths {
        failures.push(CheckoutPathFailure {
            path,
            error: GitError::Exit(1),
        });
    }
    let mut delayed_updates = std::mem::take(&mut delayed_finish.updates);
    restored.extend(delayed_updates.keys().cloned());
    for (position, entry) in index.entries.iter().enumerate() {
        if let Some(updated) = delayed_updates.remove(entry.path.as_bytes()) {
            refreshed.insert(position, updated);
        }
    }

    for (position, entry) in refreshed {
        index.entries[position] = entry;
    }
    if !index.entries.is_empty() {
        write_repository_index_ref(git_dir, format, &index)?;
    }
    Ok(CheckoutIndexPathOutcome {
        restored: restored.len(),
        failures,
    })
}

pub fn unresolve_index_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<()> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(());
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    checkout_unmerge_resolve_undo_paths(worktree_root, &mut index, format, paths)?;
    write_repository_index_ref(git_dir, format, &index)
}

pub(crate) fn checkout_selected_index_paths(
    worktree_root: &Path,
    index: &Index,
    paths: &[PathBuf],
) -> Result<BTreeSet<Vec<u8>>> {
    let index_paths = index
        .entries
        .iter()
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect::<BTreeSet<_>>();
    checkout_selected_paths(
        worktree_root,
        paths,
        index_paths.iter().map(Vec::as_slice),
        false,
    )
}

#[derive(Debug)]
struct CheckoutPathspec {
    display: String,
    element: PathspecElement,
    matched: bool,
}

#[derive(Debug)]
struct CheckoutPathspecs {
    specs: Vec<CheckoutPathspec>,
    have_include: bool,
}

impl CheckoutPathspecs {
    fn parse(worktree_root: &Path, paths: &[PathBuf]) -> Result<Self> {
        let mut specs = Vec::with_capacity(paths.len());
        let mut have_include = false;
        for path in paths {
            let git_path = checkout_pathspec_pattern(worktree_root, path)?;
            let element = PathspecElement::parse(&git_path, PathspecMatchMagic::default())
                .map_err(|err| GitError::Command(format!("bad pathspec: {err}")))?;
            have_include |= !element.is_exclude();
            specs.push(CheckoutPathspec {
                display: path.display().to_string(),
                element,
                matched: false,
            });
        }
        Ok(Self {
            specs,
            have_include,
        })
    }

    fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    fn matches(&mut self, candidate: &[u8]) -> bool {
        if self.specs.is_empty() {
            return true;
        }

        let mut included = false;
        let mut excluded = false;
        for spec in &mut self.specs {
            if spec.element.matches_path(candidate) {
                spec.matched = true;
                if spec.element.is_exclude() {
                    excluded = true;
                } else {
                    included = true;
                }
            }
        }

        !excluded && (!self.have_include || included)
    }

    fn require_matched_includes(&self, allow_unmatched: bool) -> Result<()> {
        if allow_unmatched {
            return Ok(());
        }
        if let Some(spec) = self
            .specs
            .iter()
            .find(|spec| !spec.element.is_exclude() && !spec.matched)
        {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                spec.display
            );
            return Err(GitError::Exit(1));
        }
        Ok(())
    }
}

fn checkout_pathspec_pattern(worktree_root: &Path, path: &Path) -> Result<Vec<u8>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        worktree_root.join(path)
    };
    let absolute = normalize_absolute_path_lexically(&absolute);
    let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
    })?;
    git_path_bytes(relative)
}

fn checkout_selected_paths<'a, I>(
    worktree_root: &Path,
    paths: &[PathBuf],
    candidates: I,
    allow_unmatched: bool,
) -> Result<BTreeSet<Vec<u8>>>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut pathspecs = CheckoutPathspecs::parse(worktree_root, paths)?;
    if pathspecs.is_empty() {
        return Ok(BTreeSet::new());
    }

    let mut selected = BTreeSet::new();
    for candidate in candidates {
        if pathspecs.matches(candidate) {
            selected.insert(candidate.to_vec());
        }
    }
    pathspecs.require_matched_includes(allow_unmatched)?;
    Ok(selected)
}

fn checkout_selected_positions<'a, I>(
    worktree_root: &Path,
    paths: &[PathBuf],
    candidates: I,
    allow_unmatched: bool,
) -> Result<BTreeSet<usize>>
where
    I: IntoIterator<Item = (usize, &'a [u8])>,
{
    let mut pathspecs = CheckoutPathspecs::parse(worktree_root, paths)?;
    if pathspecs.is_empty() {
        return Ok(BTreeSet::new());
    }

    let mut selected = BTreeSet::new();
    for (position, candidate) in candidates {
        if pathspecs.matches(candidate) {
            selected.insert(position);
        }
    }
    pathspecs.require_matched_includes(allow_unmatched)?;
    Ok(selected)
}

pub(crate) fn checkout_unmerge_resolve_undo_paths(
    worktree_root: &Path,
    index: &mut Index,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<()> {
    let records = parse_resolve_undo_records(index.extension(b"REUC")?, format)?;
    if records.is_empty() {
        return Ok(());
    }
    let mut pathspecs = CheckoutPathspecs::parse(worktree_root, paths)?;
    let mut remaining = Vec::new();
    let mut unmerged_any = false;
    for record in records {
        if pathspecs.matches(&record.path) {
            remove_index_entries_with_path(&mut index.entries, &record.path);
            for (idx, stage) in record.stages.into_iter().enumerate() {
                let Some((mode, oid)) = stage else {
                    continue;
                };
                index.entries.push(resolve_undo_index_entry(
                    record.path.clone(),
                    mode,
                    oid,
                    (idx + 1) as u16,
                ));
            }
            unmerged_any = true;
        } else {
            remaining.push(record);
        }
    }
    if unmerged_any {
        index.entries.sort_by(compare_index_key);
        normalize_index_version_for_extended_flags(index);
        set_resolve_undo_extension(index, &remaining)?;
    }
    Ok(())
}

pub(crate) fn resolve_undo_index_entry(
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
    stage: u16,
) -> IndexEntry {
    let name_len = (path
        .len()
        .min(sley_index::INDEX_FLAG_NAME_LENGTH_MASK as usize)) as u16;
    IndexEntry {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: name_len | (stage << 12),
        flags_extended: 0,
        path: path.into(),
    }
}

pub(crate) fn checkout_path_is_unmerged(index: &Index, path: &[u8]) -> bool {
    index
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes() == path && entry.stage() != Stage::Normal)
}

pub(crate) fn checkout_merge_unmerged_path(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    index: &Index,
    positions: &[usize],
    style: CheckoutConflictStyle,
) -> Result<()> {
    let mut base = None;
    let mut ours = None;
    let mut theirs = None;
    for position in positions {
        let entry = &index.entries[*position];
        match entry.stage() {
            Stage::Base => base = Some(entry),
            Stage::Ours => ours = Some(entry),
            Stage::Theirs => theirs = Some(entry),
            Stage::Normal => {}
        }
    }
    let Some(ours) = ours else {
        return Ok(());
    };
    let Some(theirs) = theirs else {
        return Ok(());
    };
    let base_body = match base {
        Some(entry) => read_expected_object(db, &entry.oid, ObjectType::Blob)?
            .body
            .clone(),
        None => Vec::new(),
    };
    let ours_body = read_expected_object(db, &ours.oid, ObjectType::Blob)?
        .body
        .clone();
    let theirs_body = read_expected_object(db, &theirs.oid, ObjectType::Blob)?
        .body
        .clone();
    let result = sley_diff_merge::merge_blobs(
        &base_body,
        &ours_body,
        &theirs_body,
        &sley_diff_merge::MergeBlobOptions {
            ours_label: "ours",
            theirs_label: "theirs",
            base_label: "base",
            style: match style {
                CheckoutConflictStyle::Merge => sley_diff_merge::ConflictStyle::Merge,
                CheckoutConflictStyle::Diff3 => sley_diff_merge::ConflictStyle::Diff3,
            },
            favor: sley_diff_merge::MergeFavor::None,
            ws_ignore: sley_diff_merge::WsIgnore::EMPTY,
            marker_size: 7,
        },
    );
    let file_path = worktree_path(worktree_root, ours.path.as_bytes())?;
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    remove_existing_worktree_path(&file_path)?;
    fs::write(&file_path, result.content)?;
    set_worktree_file_mode(&file_path, ours.mode)?;
    Ok(())
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
    let head_directories = match resolve_head_tree_oid(git_dir, format, &db)? {
        Some(tree_oid) => tree_directory_entries(&db, format, &tree_oid)?,
        None => BTreeMap::new(),
    };
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &head_entries,
        &head_directories,
        paths,
        false,
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
    let source_directories = tree_directory_entries(&db, format, tree_oid)?;
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
        &source_directories,
        paths,
        false,
    )
}

pub fn restore_index_paths_from_tree_allow_unmatched(
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
    let source_directories = tree_directory_entries(&db, format, tree_oid)?;
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
        &source_directories,
        paths,
        true,
    )
}

pub(crate) fn restore_index_paths_from_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    mut index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    source_directories: &BTreeMap<Vec<u8>, ObjectId>,
    paths: &[PathBuf],
    allow_unmatched: bool,
) -> Result<RestoreResult> {
    let sparse = active_sparse_checkout(git_dir)?;
    // Select against both the sparse index boundaries and the flattened source
    // tree before deciding whether any collapsed directory must be opened. An
    // in-cone path such as `deep/a` is already an explicit index entry, so a
    // reset of that path must not inflate unrelated out-of-cone directories.
    let matched_paths = checkout_selected_paths(
        worktree_root,
        paths,
        index
            .entries
            .iter()
            .map(|entry| entry.path.as_bytes())
            .chain(source_entries.keys().map(Vec::as_slice))
            .chain(source_directories.keys().map(Vec::as_slice)),
        allow_unmatched,
    )?;
    let selected_sparse_boundaries = index
        .entries
        .iter()
        .filter(|entry| entry.is_sparse_dir() && matched_paths.contains(entry.path.as_bytes()))
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect::<BTreeSet<_>>();
    let selected_inside_sparse_directory = matched_paths
        .iter()
        .filter(|path| {
            !selected_sparse_boundaries
                .iter()
                .any(|boundary| path.as_slice() != boundary && path.starts_with(boundary))
        })
        .any(|path| index_sparse_dir_contains_path(&index, path));
    if selected_inside_sparse_directory {
        // The current per-leaf reset implementation needs the selected sparse
        // subtree in full. Keep the conservative fallback for those pathspecs;
        // restrictive in-cone pathspecs avoid it entirely.
        expand_sparse_index(&mut index, db, format)?;
    }
    let index_version = index.version;
    let extensions = index_extensions_without_cache_tree(&index.extensions);
    let mut index_entries = index
        .entries
        .into_iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    let prior_skip_worktree = index_entries
        .iter()
        .filter(|(_, entry)| entry.is_skip_worktree())
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    let mut restored = BTreeSet::new();
    for boundary in &selected_sparse_boundaries {
        if let Some(source_oid) = source_directories.get(boundary) {
            if let Some(entry) = index_entries.get_mut(boundary) {
                entry.mode = SPARSE_DIR_MODE;
                entry.oid = *source_oid;
                entry.set_skip_worktree(true);
            }
        } else {
            index_entries.remove(boundary);
        }
        restored.insert(boundary.clone());
    }
    for path in matched_paths {
        if selected_sparse_boundaries
            .iter()
            .any(|boundary| path.as_slice() == boundary || path.as_slice().starts_with(boundary))
        {
            continue;
        }
        if let Some(entry) = source_entries.get(&path) {
            // git's pathspec reset (`reset_index` → diff against the source
            // tree) only rewrites entries that actually CHANGE: an entry whose
            // oid and mode already equal the source is left untouched, so its
            // cached stat is preserved and `git diff-files` stays clean (t7102
            // "resetting an unmodified path is a no-op"). Only when the entry
            // genuinely changes does git write a fresh, stat-zeroed entry.
            let unchanged = index_entries.get(&path).is_some_and(|existing| {
                existing.oid == entry.oid
                    && existing.mode == entry.mode
                    && !existing.is_intent_to_add()
            });
            if !unchanged {
                let mut restored = restored_head_index_entry(worktree_root, db, &path, entry)?;
                if prior_skip_worktree.contains(&path) {
                    restored.set_skip_worktree(true);
                }
                index_entries.insert(path.clone(), restored);
            }
        } else {
            index_entries.remove(&path);
        }
        restored.insert(path);
    }
    let mut entries = index_entries.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let restored_paths = restored.iter().cloned().collect::<Vec<_>>();
    let mut index = Index {
        version: index_version,
        entries,
        extensions,
        checksum: None,
    };
    invalidate_untracked_cache_for_git_paths(&mut index, format, &restored_paths)?;
    if let Some((sparse, mode)) = sparse
        && sparse.sparse_index
    {
        if index.entries.iter().any(IndexEntry::is_sparse_dir) {
            // No selected path entered a collapsed boundary, so the existing
            // sparse-directory entries remain authoritative. Preserve that
            // layout directly instead of expanding merely to re-collapse it.
            index.set_sparse_extension();
            normalize_index_version_for_extended_flags(&mut index);
        } else {
            let matcher = SparseMatcher::new(&sparse, mode);
            collapse_to_sparse_index(&mut index, &matcher, db, format)?;
        }
    }
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

/// Return every directory entry in `tree_oid`, keyed with the trailing slash
/// spelling used by sparse-index directory entries.
fn tree_directory_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, ObjectId>> {
    fn collect(
        db: &FileObjectDatabase,
        format: ObjectFormat,
        tree_oid: &ObjectId,
        prefix: &[u8],
        directories: &mut BTreeMap<Vec<u8>, ObjectId>,
    ) -> Result<()> {
        let object = read_expected_object(db, tree_oid, ObjectType::Tree)?;
        for entry in Tree::parse(format, &object.body)?.entries {
            if tree_entry_object_type(entry.mode) != ObjectType::Tree {
                continue;
            }
            let mut path = prefix.to_vec();
            path.extend_from_slice(entry.name.as_bytes());
            path.push(b'/');
            directories.insert(path.clone(), entry.oid);
            collect(db, format, &entry.oid, &path, directories)?;
        }
        Ok(())
    }

    let mut directories = BTreeMap::new();
    collect(db, format, tree_oid, b"", &mut directories)?;
    Ok(directories)
}

pub fn restore_index_and_worktree_paths_from_head(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    overlay: bool,
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
        overlay,
    )
}

pub fn restore_index_and_worktree_paths_from_tree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    paths: &[PathBuf],
    overlay: bool,
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
        overlay,
    )
}

pub(crate) fn restore_index_and_worktree_paths_from_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    mut index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: &[PathBuf],
    overlay: bool,
) -> Result<RestoreResult> {
    let sparse = active_sparse_checkout(git_dir)?;
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    let mut restored = BTreeSet::new();
    let candidate_paths = index
        .entries
        .iter()
        .map(|entry| entry.path.as_bytes().to_vec())
        .chain(source_entries.keys().cloned())
        .collect::<BTreeSet<_>>();
    let matched_paths = checkout_selected_paths(
        worktree_root,
        paths,
        candidate_paths.iter().map(Vec::as_slice),
        false,
    )?;
    let selected_sparse_boundaries = index
        .entries
        .iter()
        .filter(|entry| {
            entry.is_sparse_dir()
                && matched_paths.iter().any(|path| {
                    path.as_slice() == entry.path.as_bytes()
                        || path.starts_with(entry.path.as_bytes())
                })
        })
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect::<BTreeSet<_>>();
    let expanded_sparse_directories =
        expand_sparse_index_directories(&mut index, db, format, |boundary| {
            selected_sparse_boundaries.contains(boundary)
        })?;
    let index_version = index.version;
    let extensions = index_extensions_without_cache_tree(&index.extensions);
    let mut replacement_entries = Vec::new();
    let mut replaced_paths = BTreeSet::new();
    let mut replacement_leaf_paths = BTreeSet::new();
    for path in matched_paths {
        if let Some(entry) = source_entries.get(&path) {
            replacement_entries.push(materialize_path_restore_entry_filtered(
                db,
                format,
                worktree_root,
                git_dir,
                &path,
                entry,
                &config,
            )?);
            replacement_leaf_paths.insert(path.clone());
        } else if overlay {
            // Overlay mode (git checkout default): a path that matches the
            // pathspec but is absent from the source tree is left untouched
            // in both the index and the working tree.
            continue;
        } else {
            // No-overlay mode (git restore default, checkout --no-overlay):
            // drop the path from the index and the working tree.
            remove_worktree_file(worktree_root, &path)?;
        }
        replaced_paths.insert(path.clone());
        restored.insert(path);
    }
    // A tree path checkout resolves every stage of a selected path to exactly
    // one source entry. Apply the batch at the entry-vector boundary so an
    // expanded sparse-directory leaf cannot survive beside its replacement,
    // while unselected conflict stages remain byte-for-byte intact.
    index.entries.retain(|entry| {
        !replaced_paths.contains(entry.path.as_bytes())
            && !replacement_leaf_paths
                .iter()
                .any(|replacement| checkout_paths_collide(entry.path.as_bytes(), replacement))
    });
    index.entries.extend(replacement_entries);
    index.entries.sort_by(compare_index_key);
    let restored_paths = restored.iter().cloned().collect::<Vec<_>>();
    let mut index = Index {
        version: index_version,
        entries: index.entries,
        extensions,
        checksum: None,
    };
    invalidate_untracked_cache_for_git_paths(&mut index, format, &restored_paths)?;
    if let Some((sparse, mode)) = sparse
        && sparse.sparse_index
    {
        if expanded_sparse_directories || !index.entries.iter().any(IndexEntry::is_sparse_dir) {
            let matcher = SparseMatcher::new(&sparse, mode);
            collapse_to_sparse_index(&mut index, &matcher, db, format)?;
        } else {
            index.set_sparse_extension();
            normalize_index_version_for_extended_flags(&mut index);
        }
    }
    write_repository_index_ref(git_dir, format, &index)?;
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
    let _process_filter_cwd = set_process_filter_cwd(Some(worktree_root.to_path_buf()));
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, commit_oid)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;
    let sparse = active_sparse_checkout(git_dir)?;
    let sparse_matcher = sparse
        .as_ref()
        .map(|(spec, mode)| SparseMatcher::new(spec, *mode));
    let prior_expanded_sparse_directories =
        if sparse.as_ref().is_some_and(|(spec, _)| spec.sparse_index) {
            match fs::read(repository_index_path(git_dir)) {
                Ok(bytes) => sparse_index_expanded_boundaries(
                    &Index::parse(&bytes, format)?,
                    sparse_matcher.as_ref(),
                ),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
                Err(err) => return Err(err.into()),
            }
        } else {
            BTreeSet::new()
        };
    refuse_if_current_working_directory_becomes_file(worktree_root, &target_entries)?;
    let config = effective_worktree_config(git_dir, None).unwrap_or_default();
    let attributes = build_tree_attribute_matcher(worktree_root, &db, format, &commit.tree)?;

    // git's `reset --hard` runs a one-way merge through unpack-trees: EVERY path
    // present in the current index (at ANY stage) that the target tree does not
    // track is removed from the worktree. A conflicted D/F merge can leave a
    // path like `dir~HEAD` at stage 2 only — those entries are dropped by the
    // stage-0-only `read_index_entries`, so iterate the RAW index paths here
    // (deduped across stages) to match git and delete the moved-aside file.
    for path in current_index_paths(git_dir, format, &db)? {
        if !target_entries.contains_key(&path) {
            remove_worktree_file(worktree_root, &path)?;
        }
    }

    let mut index_entries = Vec::new();
    let mut prepared_entries = Vec::new();
    let mut delayed_checkout = DelayedCheckoutQueue::default();
    for (path, entry) in &target_entries {
        if sparse_matcher
            .as_ref()
            .is_some_and(|matcher| !matcher.includes_file(path))
        {
            // `reset --hard` still runs through unpack-trees' sparse-checkout
            // filter. Out-of-cone paths remain represented in the index, but
            // are not materialized in the worktree and carry CE_SKIP_WORKTREE.
            // Rebuilding every target path unconditionally made a sparse-index
            // reset appear as a mass deletion to status after those files were
            // removed again by the next sparse-aware command.
            remove_worktree_file(worktree_root, path)?;
            let mut skipped = restored_head_index_entry(worktree_root, &db, path, entry)?;
            skipped.set_skip_worktree(true);
            index_entries.push(skipped);
        } else {
            match prepare_checkout_entry(
                &db,
                format,
                path,
                entry,
                Some(&config),
                Some(&attributes),
                &mut delayed_checkout,
            )? {
                PreparedCheckoutResult::Ready(prepared) => prepared_entries.push(prepared),
                PreparedCheckoutResult::Delayed(entry) => index_entries.push(entry),
            }
        }
    }
    index_entries.extend(materialize_prepared_checkout_entries(
        worktree_root,
        &config,
        prepared_entries,
    )?);
    let mut delayed_updates = finish_delayed_checkout(worktree_root, delayed_checkout)?;
    for entry in &mut index_entries {
        if let Some(updated) = delayed_updates.remove(entry.path.as_bytes()) {
            *entry = updated;
        }
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let extensions = preserved_index_extensions(git_dir, format)?;
    let mut index = Index {
        version: 2,
        entries: index_entries,
        extensions,
        checksum: None,
    };
    // `sdir` describes the old entry table. Rebuild it only after the new full
    // target index has been classified; carrying the marker across a rebuild
    // produces an invalid "full entries + sparse extension" hybrid.
    index.clear_sparse_extension()?;
    normalize_index_version_for_extended_flags(&mut index);
    if let (Some((spec, _)), Some(matcher)) = (sparse.as_ref(), sparse_matcher.as_ref())
        && spec.sparse_index
    {
        collapse_to_sparse_index(&mut index, matcher, &db, format)?;
        if !prior_expanded_sparse_directories.is_empty() {
            expand_sparse_index_directories_impl(
                &mut index,
                &db,
                format,
                |prefix| prior_expanded_sparse_directories.contains(prefix),
                false,
            )?;
        }
    }
    refresh_cache_tree(&mut index, &db);
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(RestoreResult {
        restored: target_entries.len(),
    })
}

pub fn reset_index_and_worktree_to_commit_with_process_filter_metadata(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    commit_oid: &ObjectId,
    process_metadata: Option<ProcessFilterMetadata>,
) -> Result<RestoreResult> {
    let _process_filter_metadata = set_process_filter_metadata(process_metadata);
    reset_index_and_worktree_to_commit(worktree_root, git_dir, format, commit_oid)
}

/// All paths the current index references, deduped across stages (a conflicted
/// path appears at stages 1–3; we want it listed once). Unlike
/// `read_index_entries`, which filters to stage 0, this keeps conflicted paths
/// so a `reset --hard` worktree sweep removes moved-aside files (`dir~HEAD`) the
/// target tree doesn't track — matching git's one-way unpack-trees behavior.
pub(crate) fn current_index_paths(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<BTreeSet<Vec<u8>>> {
    let (index, _stat_cache, _head_matches) = read_index_with_stat_cache(git_dir, format, db)?;
    Ok(index
        .entries
        .into_iter()
        .map(|entry| entry.path.into_bytes())
        .collect())
}

/// Write one target tree entry into the worktree and return its index entry —
/// the shared materialization step for every checkout/reset worktree rebuild.
///
/// Gitlinks (mode 160000) never touch the object database: their oid names a
/// commit in the *submodule's* repository, not an object here. Upstream
/// (entry.c `write_entry` S_IFGITLINK) just mkdirs the path — an
/// already-populated submodule is left untouched (EEXIST is success) — and
/// records the oid in the index with a zeroed stat so status re-evaluates the
/// gitlink against the embedded repository's HEAD.
pub(crate) fn materialize_tree_entry(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<IndexEntry> {
    if sley_index::is_gitlink(entry.mode) {
        let dir_path = worktree_path(worktree_root, path)?;
        materialize_gitlink_dir(worktree_root, &dir_path)?;
        return Ok(IndexEntry {
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
            flags: path.len().min(0x0fff) as u16,
            flags_extended: 0,
            path: BString::from(path),
        });
    }
    let file_path = write_worktree_blob_entry(db, worktree_root, path, entry)?;
    let metadata = fs::symlink_metadata(&file_path)?;
    let mut index_entry = index_entry_from_metadata(path.to_vec(), entry.oid, &metadata);
    index_entry.mode = entry.mode;
    Ok(index_entry)
}

pub(crate) fn materialize_gitlink_dir(worktree_root: &Path, dir_path: &Path) -> Result<()> {
    prepare_blob_parent_dirs(worktree_root, dir_path)?;
    // git's `validate_submodule_path` / entry.c: never replace a symlink with a
    // gitlink directory. Doing so would destroy the link and let a later
    // --recurse-submodules pass migrate the linked repo's .git into
    // $GIT_DIR/modules (t7423). Leave the symlink in place so the recursive
    // submodule path can refuse with the proper error.
    if let Ok(metadata) = fs::symlink_metadata(dir_path) {
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        if !metadata.is_dir() {
            remove_existing_worktree_path(dir_path)?;
        }
    }
    fs::create_dir_all(dir_path)?;
    Ok(())
}

pub(crate) fn materialize_path_restore_entry_filtered(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    worktree_root: &Path,
    git_dir: &Path,
    path: &[u8],
    entry: &TrackedEntry,
    config: &GitConfig,
) -> Result<IndexEntry> {
    if sley_index::is_gitlink(entry.mode) || (entry.mode & 0o170000) == 0o120000 {
        return materialize_tree_entry(db, worktree_root, path, entry);
    }
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    let checks = smudge_attribute_checks_from_index(worktree_root, git_dir, format, path)?;
    let body = apply_smudge_filter_with_attributes_cow_format(
        config,
        &checks,
        path,
        &object.body,
        format,
    )?;
    let file_path = worktree_path(worktree_root, path)?;
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    remove_existing_worktree_path(&file_path)?;
    fs::write(&file_path, &body)?;
    set_worktree_file_mode(&file_path, entry.mode)?;
    let metadata = fs::symlink_metadata(&file_path)?;
    let mut index_entry = index_entry_from_metadata(path.to_vec(), entry.oid, &metadata);
    index_entry.mode = entry.mode;
    Ok(index_entry)
}

/// Materialize a blob (or symlink) tree entry into the worktree at `path`,
/// returning the absolute path written. Shared by every checkout/reset worktree
/// rebuild so the type-change handling is identical everywhere.
///
/// Mirrors git's entry.c `write_entry`: it unlinks whatever currently occupies
/// the path before creating the new object, so a type transition (regular file ⇄
/// symlink, or a stale symlink/directory in the way) is overwritten rather than
/// left in place or failing with EEXIST. A plain `fs::write` follows an existing
/// symlink and would write *through* it (leaving the link), so the unlink is
/// load-bearing for the symlink-stash / reset-hard type-change cases.
pub(crate) fn write_worktree_blob_entry(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<PathBuf> {
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    let file_path = worktree_path(worktree_root, path)?;
    // Clear any non-directory blocking an ancestor component (prior tree had
    // `dir` as a FILE, target wants `dir/<child>`), creating the parent dirs.
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    // Clear whatever sits at the leaf — including a directory where the target
    // wants a plain file (reverse D/F) — before writing.
    remove_existing_worktree_path(&file_path)?;
    write_blob_body_or_symlink(&file_path, entry.mode, &object.body, &object.body)?;
    Ok(file_path)
}

/// Write the materialized worktree object at `file_path` as the right *type* for
/// `mode` — git's `entry.c` `write_entry` type-by-mode switch, factored into a
/// single primitive so no checkout/reset/restore materializer can silently write
/// a symlink blob as a regular file (the symlink-checkout bug class).
///
/// The caller is responsible for the pre-write steps (leading directories +
/// removing any blocker at the leaf). Type by `mode`:
/// * `0o120000` (symlink) → a real symlink whose target is `link_target`, the
///   **raw** blob bytes. git treats symlink content as an opaque path, so the
///   smudge/EOL filter never applies — pass the unfiltered blob here even when
///   `body` is the smudged content for the regular-file arm.
/// * everything else → a regular file holding `body`, with the user-execute bit
///   set iff `mode` has it (`set_worktree_file_mode`).
///
/// Exposed crate-publicly so out-of-crate worktree materializers (e.g.
/// `sley-cli`'s `stash -u` untracked-tree restore) route through the same
/// type-by-mode primitive instead of re-deriving an `fs::write` that drops the
/// symlink arm.
pub fn write_blob_body_or_symlink(
    file_path: &Path,
    mode: u32,
    body: &[u8],
    link_target: &[u8],
) -> Result<()> {
    if (mode & 0o170000) == 0o120000 {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let target =
                std::path::PathBuf::from(std::ffi::OsString::from_vec(link_target.to_vec()));
            std::os::unix::fs::symlink(&target, file_path)?;
        }
        #[cfg(not(unix))]
        {
            let _ = link_target;
            fs::write(file_path, body)?;
        }
    } else {
        fs::write(file_path, body)?;
        set_worktree_file_mode(file_path, mode)?;
    }
    Ok(())
}

/// Create the ancestor directories of a worktree blob path, removing any
/// regular file or symlink that occupies an ancestor *component* first.
///
/// Mirrors git's `entry.c` `create_directories`: it walks each path component
/// between `worktree_root` and the leaf and, for each, if a non-directory (a
/// regular file or symlink left by a prior tree where `dir` was a FILE) blocks
/// it, unlinks the blocker before `mkdir`. A plain `fs::create_dir_all` fails
/// with `ENOTDIR`/`EEXIST` on such a D/F transition; this is the directory-side
/// of git's force-checkout D/F clearing.
///
/// `worktree_root` itself is never touched. Only components strictly between the
/// root and the leaf are cleared, matching `create_directories`' `base_dir_len`
/// boundary.
pub(crate) fn prepare_blob_parent_dirs(worktree_root: &Path, file_path: &Path) -> Result<()> {
    let parent = match file_path.parent() {
        Some(parent) => parent,
        None => return Ok(()),
    };
    // Fast path: parent already is a directory (the overwhelmingly common
    // case).  Do not use `Path::is_dir()` here: it follows a symlink.  A
    // checkout of `D/file` with an untracked `D -> elsewhere` must replace the
    // link with a real directory, never write through it into `elsewhere`.
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => return Ok(()),
        Ok(_) => {}
        // `lstat("file/child")` reports ENOTDIR when an earlier component is
        // the D/F blocker we are about to replace. Treat it like an absent
        // descendant and let the root-to-leaf walk remove that blocker.
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => {}
        Err(err) => return Err(err.into()),
    }
    // Collect the ancestor chain from worktree_root (exclusive) down to `parent`
    // (inclusive). We can't `create_dir_all` blindly because a non-directory may
    // sit on one of these components; walk them and clear blockers as git does.
    let mut components: Vec<&Path> = Vec::new();
    let mut cursor = Some(parent);
    while let Some(dir) = cursor {
        if dir == worktree_root {
            break;
        }
        components.push(dir);
        cursor = dir.parent();
        if cursor.is_none() {
            break;
        }
    }
    // Walk root → leaf so each parent exists before its child.
    for dir in components.iter().rev() {
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                // A regular file or symlink occupies this component (the prior
                // tree had `dir` as a FILE). Unlink it, then create the dir.
                fs::remove_file(dir)?;
                fs::create_dir(dir)?;
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                fs::create_dir(dir)?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// Remove whatever currently occupies a worktree path before writing a new
/// object there — a symlink (even a dangling one, which `Path::exists` misses),
/// a regular file, or a directory subtree. Uses `symlink_metadata` (lstat) so a
/// symlink is removed as the link, never followed.
pub(crate) fn remove_existing_worktree_path(file_path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(file_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if metadata.is_dir() {
        if path_is_original_cwd(file_path) {
            return refuse_remove_current_working_directory(file_path);
        }
        // A directory in the way of a file (D/F transition) or a populated
        // gitlink: remove the subtree so the file can be created.
        match fs::remove_dir_all(file_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    } else {
        fs::remove_file(file_path)?;
    }
    Ok(())
}

/// chmod a freshly-materialized worktree blob to match its tree/index entry mode.
///
/// `fs::write` truncates an existing file *in place*, preserving its prior
/// permission bits. For a mode-only diff (identical oid, 100644 vs 100755) that
/// leaves the wrong exec bit on disk — which is exactly the `reset --hard` /
/// checkout bug this guards against. git's checkout path unlinks+recreates the
/// file precisely to "get the new one with the right permissions" (entry.c
/// `write_entry`); we instead chmod the just-written file.
///
/// Mirrors the observable result of git's `create_file` (entry.c):
/// `(mode & 0100) ? 0777 : 0666` masked by the standard umask (0022), i.e. 0755
/// for an executable entry and 0644 otherwise. Only regular-file entries (100644
/// / 100755) are chmod'd; gitlinks and symlinks have no meaningful exec bit.
///
/// We set the perms directly (rather than relying on a fresh `open(2)` to apply
/// the umask) because `fs::write` truncates an existing file in place, leaving its
/// old permission bits — the very thing that breaks a mode-only checkout/reset.
/// Matching git's default-umask output keeps the worktree byte-for-byte aligned
/// with the oracle, which is what the parity suite asserts.
#[cfg(unix)]
pub(crate) fn set_worktree_file_mode(file_path: &Path, entry_mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = match entry_mode {
        0o100755 => 0o755,
        0o100644 => 0o644,
        _ => return Ok(()),
    };
    fs::set_permissions(file_path, fs::Permissions::from_mode(perms))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_worktree_file_mode(_file_path: &Path, _entry_mode: u32) -> Result<()> {
    Ok(())
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
        index_entries.push(materialize_tree_entry(&db, worktree_root, path, entry)?);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let extensions = preserved_index_extensions(git_dir, format)?;
    let mut index = Index {
        version: 2,
        entries: index_entries,
        extensions,
        checksum: None,
    };
    refresh_cache_tree(&mut index, &db);
    write_repository_index_ref(git_dir, format, &index)?;
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
    // git's `reset --mixed` reuses index entries that already match the target
    // tree. In particular, their validated stat tuple survives `--no-refresh`;
    // only newly introduced or object/mode-changed entries stay stat-dirty.
    // It also preserves the skip-worktree bit on surviving entries.
    let sparse = active_sparse_checkout(git_dir)?;
    let sparse_matcher = sparse
        .as_ref()
        .map(|(spec, mode)| SparseMatcher::new(spec, *mode));
    let index_path = repository_index_path(git_dir);
    let prior_entries: BTreeMap<Vec<u8>, IndexEntry> = match fs::read(&index_path) {
        Ok(bytes) => {
            let mut prior = Index::parse(&bytes, format)?;
            // A sparse-directory entry records only `folder/`; mixed reset
            // preserves skip-worktree per leaf. Expand first so every path
            // beneath the directory contributes its previous bit.
            if prior.entries.iter().any(IndexEntry::is_sparse_dir) {
                expand_sparse_index_in_memory(&mut prior, &db, format)?;
            }
            prior
                .entries
                .into_iter()
                .map(|entry| (entry.path.as_bytes().to_vec(), entry))
                .collect()
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(err) => return Err(err.into()),
    };
    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        let mut restored = restored_head_index_entry(worktree_root, &db, path, entry)?;
        if let Some(prior) = prior_entries.get(path)
            && prior.oid == restored.oid
            && prior.mode == restored.mode
            && prior.stage() == sley_index::Stage::Normal
        {
            restored.ctime_seconds = prior.ctime_seconds;
            restored.ctime_nanoseconds = prior.ctime_nanoseconds;
            restored.mtime_seconds = prior.mtime_seconds;
            restored.mtime_nanoseconds = prior.mtime_nanoseconds;
            restored.dev = prior.dev;
            restored.ino = prior.ino;
            restored.uid = prior.uid;
            restored.gid = prior.gid;
            restored.size = prior.size;
        }
        // Preserve skip-worktree on surviving entries the way git's mixed reset
        // does. Do *not* force the bit from the current sparse patterns onto an
        // entry that previously cleared it (e.g. a present out-of-cone file that
        // sparse-checkout left as "not up to date" — t3705 #16 relies on
        // `git reset` keeping such an entry non-skip-worktree so a later
        // `add --sparse --renormalize` can stage it). New out-of-cone paths that
        // had no prior entry still receive the bit.
        let prior = prior_entries.get(path);
        let out_of_cone = sparse_matcher
            .as_ref()
            .is_some_and(|matcher| !matcher.includes_file(path));
        if prior.is_some_and(IndexEntry::is_skip_worktree)
            || (out_of_cone && prior.is_none())
        {
            restored.set_skip_worktree(true);
        }
        index_entries.push(restored);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut index = Index {
        version: 2,
        entries: index_entries,
        extensions: preserved_index_extensions(git_dir, format)?,
        checksum: None,
    };
    index.clear_sparse_extension()?;
    normalize_index_version_for_extended_flags(&mut index);
    if let (Some((spec, _)), Some(matcher)) = (sparse.as_ref(), sparse_matcher.as_ref())
        && spec.sparse_index
    {
        collapse_to_sparse_index(&mut index, matcher, &db, format)?;
    }
    refresh_cache_tree(&mut index, &db);
    write_repository_index_ref(git_dir, format, &index)?;
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
/// Returns `true` when `path` is inside the sparse-checkout described by
/// `sparse` under the given matching `mode`. This is the engine behind
/// `git sparse-checkout check-rules`: a path is "in" the sparse-checkout when
/// the compiled matcher would keep its worktree file. Cone and full (gitignore)
/// grammars are both handled, exactly as the apply engine interprets them, so
/// `check-rules` and `set`/`reapply` agree by construction.
pub fn path_in_sparse_checkout(
    path: &[u8],
    sparse: &SparseCheckout,
    mode: SparseCheckoutMode,
) -> bool {
    SparseMatcher::new(sparse, mode).includes_file(path)
}

pub fn active_sparse_checkout(
    git_dir: &Path,
) -> Result<Option<(SparseCheckout, SparseCheckoutMode)>> {
    let worktree_config = GitConfig::read(git_dir.join("config.worktree")).unwrap_or_default();
    let repo_config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    let sparse_enabled = worktree_config
        .get_bool("core", None, "sparseCheckout")
        .or_else(|| repo_config.get_bool("core", None, "sparseCheckout"))
        .unwrap_or(false);
    if !sparse_enabled {
        return Ok(None);
    }
    let sparse_file = git_dir.join("info").join("sparse-checkout");
    if !sparse_file.exists() {
        return Ok(None);
    }
    let cone = worktree_config
        .get_bool("core", None, "sparseCheckoutCone")
        .or_else(|| repo_config.get_bool("core", None, "sparseCheckoutCone"))
        .unwrap_or(false);
    let sparse_index = cone
        && worktree_config
            .get_bool("index", None, "sparse")
            .or_else(|| repo_config.get_bool("index", None, "sparse"))
            .unwrap_or(false);
    let bytes = fs::read(sparse_file)?;
    let mut patterns = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if patterns.last().map(Vec::is_empty) == Some(true) {
        patterns.pop();
    }
    let mode = if cone {
        SparseCheckoutMode::Cone
    } else {
        SparseCheckoutMode::Full
    };
    Ok(Some((
        SparseCheckout {
            patterns,
            sparse_index,
        },
        mode,
    )))
}

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
            unmerged: Vec::new(),
            untracked_sparse_directories: Vec::new(),
        });
    };
    let matcher = SparseMatcher::new(sparse, mode);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    // Expand any collapsed sparse-directory entries to a full index before we
    // reconcile per-path: the apply loop reasons about individual blob paths, so
    // it must never see a sparse-dir entry. (Re-collapse happens at the end when
    // a sparse index is requested.)
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        if sparse.sparse_index {
            expand_sparse_index_in_memory(&mut index, &db, format)?;
        } else {
            expand_sparse_index(&mut index, &db, format)?;
        }
    }
    let mut materialized = Vec::new();
    let mut skipped = Vec::new();
    let mut not_up_to_date = Vec::new();
    let mut unmerged = Vec::new();
    for entry in &mut index.entries {
        // Never touch conflicted entries.
        if index_entry_stage(entry) != 0 {
            unmerged.push(entry.path.as_bytes().to_vec());
            continue;
        }
        if matcher.includes_file(entry.path.as_bytes()) {
            clear_skip_worktree(entry);
            let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
            if !file_path.exists() {
                materialize_index_entry_file(&db, worktree_root, &file_path, entry)?;
                let metadata = fs::symlink_metadata(&file_path)?;
                *entry = index_entry_with_refreshed_stat(entry, &metadata);
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
                Ok(metadata)
                    if !sparse_checkout_worktree_entry_is_uptodate(
                        worktree_root,
                        git_dir,
                        format,
                        entry,
                        &metadata,
                    )? =>
                {
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
    unmerged.sort();
    unmerged.dedup();
    let untracked_sparse_directories = if matches!(mode, SparseCheckoutMode::Cone) {
        clean_tracked_sparse_directories(worktree_root, &index)?
    } else {
        Vec::new()
    };
    normalize_index_version_for_extended_flags(&mut index);
    // When a sparse index was requested (cone mode + index.sparse), collapse the
    // fully-out-of-cone directories into single sparse-directory entries and
    // mark the index with the `sdir` extension. Otherwise ensure the index is
    // written full (and any prior `sdir` marker is cleared).
    if sparse.sparse_index {
        collapse_to_sparse_index(&mut index, &matcher, &db, format)?;
    } else {
        index.clear_sparse_extension()?;
    }
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(ApplySparseResult {
        materialized,
        skipped,
        not_up_to_date,
        unmerged,
        untracked_sparse_directories,
    })
}

/// Removes the shallowest tracked directories that are wholly marked
/// skip-worktree. Ignored leftovers are removed with such a directory, while a
/// non-ignored untracked file preserves the directory and is reported to the
/// caller. This mirrors sparse-checkout's temporary sparse-index cleanup pass.
fn clean_tracked_sparse_directories(worktree_root: &Path, index: &Index) -> Result<Vec<Vec<u8>>> {
    let mut directory_blocked = BTreeMap::<Vec<u8>, bool>::new();
    for entry in &index.entries {
        if index_entry_stage(entry) != 0 {
            continue;
        }
        let path = entry.path.as_bytes();
        let blocks_cleanup = !entry.is_skip_worktree() || sley_index::is_gitlink(entry.mode);
        let mut start = 0usize;
        while let Some(relative) = path
            .get(start..)
            .and_then(|suffix| suffix.iter().position(|byte| *byte == b'/'))
        {
            let end = start + relative;
            let blocked = directory_blocked.entry(path[..end].to_vec()).or_default();
            *blocked |= blocks_cleanup;
            start = end + 1;
        }
    }

    let all_candidates: Vec<Vec<u8>> = directory_blocked
        .iter()
        .filter(|(_, blocked)| !**blocked)
        .map(|(directory, _)| directory.clone())
        .collect();
    let mut candidates: Vec<Vec<u8>> = all_candidates
        .iter()
        .filter(|directory| {
            !all_candidates.iter().any(|other| {
                other != *directory
                    && directory
                        .strip_prefix(other.as_slice())
                        .is_some_and(|rest| rest.first() == Some(&b'/'))
            })
        })
        .cloned()
        .collect();
    candidates.sort();

    let mut preserved = Vec::new();
    for directory in candidates {
        let absolute = worktree_path(worktree_root, &directory)?;
        if !absolute.is_dir() {
            continue;
        }
        let mut stack = vec![(absolute.clone(), directory.clone())];
        let mut has_untracked = false;
        while let Some((current, git_prefix)) = stack.pop() {
            for item in fs::read_dir(&current)? {
                let item = item?;
                let mut git_path = git_prefix.clone();
                git_path.push(b'/');
                git_path.extend_from_slice(&os_path_component_bytes(&item.file_name()));
                let file_type = item.file_type()?;
                if file_type.is_dir() {
                    stack.push((item.path(), git_path));
                } else if !path_matches_standard_ignore(worktree_root, &git_path, false)? {
                    has_untracked = true;
                    break;
                }
            }
            if has_untracked {
                break;
            }
        }
        if has_untracked {
            let mut sparse_name = directory;
            sparse_name.push(b'/');
            preserved.push(sparse_name);
        } else {
            fs::remove_dir_all(absolute)?;
        }
    }
    Ok(preserved)
}

fn os_path_component_bytes(component: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        component.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        component.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

/// Expands every sparse-directory entry in `index` back into the full set of
/// blob (and nested-directory) entries it collapses, reading each directory's
/// tree from `db`. After this the index contains no sparse-directory entries and
/// carries no `sdir` marker — it is a full index that any per-path command can
/// operate on without sparse-index awareness.
///
/// This is the **close-the-class** primitive: a command never needs to special-
/// case a sparse index, because the moment it loads the index it expands to the
/// full form. The collapsed shape is purely an on-disk storage optimization.
pub fn expand_sparse_index(
    index: &mut Index,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<bool> {
    expand_sparse_index_impl(index, db, format, true)
}

/// Expand a sparse index only as an internal semantic view. The caller either
/// preserves the on-disk sparse layout or immediately re-collapses it, so this
/// must not advertise Git's observable `ensure_full_index` transition.
pub(crate) fn expand_sparse_index_in_memory(
    index: &mut Index,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<bool> {
    expand_sparse_index_impl(index, db, format, false)
}

/// Expand a sparse index into a temporary semantic view without emitting the
/// observable `ensure_full_index` transition.
///
/// Use this for read-only matching or validation when the caller neither
/// writes the expanded index nor changes the repository's sparse layout.
pub fn expand_sparse_index_view(
    index: &mut Index,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<bool> {
    expand_sparse_index_in_memory(index, db, format)
}

/// Expand only collapsed sparse directories selected by `should_expand`.
///
/// Commands with a restrictive pathspec can operate directly on in-cone
/// entries while leaving unrelated sparse directories collapsed. A selected
/// directory is expanded into skip-worktree leaves; unselected directories and
/// the sparse-index extension remain intact. The expansion is observable in
/// trace2 exactly once when at least one directory is selected.
pub fn expand_sparse_index_directories(
    index: &mut Index,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    should_expand: impl FnMut(&[u8]) -> bool,
) -> Result<bool> {
    expand_sparse_index_directories_impl(index, db, format, should_expand, true)
}

fn expand_sparse_index_directories_impl(
    index: &mut Index,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    mut should_expand: impl FnMut(&[u8]) -> bool,
    emit_trace: bool,
) -> Result<bool> {
    if !index.entries.iter().any(IndexEntry::is_sparse_dir) {
        return Ok(false);
    }
    let mut changed = false;
    let mut expanded = Vec::with_capacity(index.entries.len());
    for entry in std::mem::take(&mut index.entries) {
        if !entry.is_sparse_dir() || !should_expand(entry.path.as_bytes()) {
            expanded.push(entry);
            continue;
        }
        changed = true;
        let prefix = entry.path.as_bytes();
        for (relative, (mode, oid)) in sley_diff_merge::flatten_tree(db, format, &entry.oid)? {
            let mut path = prefix.to_vec();
            path.extend_from_slice(&relative);
            let mut leaf = blank_sparse_blob_entry(format, &path, mode, oid);
            leaf.set_skip_worktree(true);
            expanded.push(leaf);
        }
    }
    expanded.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    index.entries = expanded;
    if !changed {
        return Ok(false);
    }
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        index.set_sparse_extension();
    } else {
        index.clear_sparse_extension()?;
    }
    normalize_index_version_for_extended_flags(index);
    if emit_trace {
        sley_core::trace2::region("index", "ensure_full_index");
    }
    Ok(true)
}

/// Identify sparse-directory boundaries that the physical index currently
/// stores as individual out-of-cone leaves. Reset refreshes those leaves but
/// must not change their expanded/collapsed representation; an explicit
/// sparse-checkout reapply owns that conversion.
fn sparse_index_expanded_boundaries(
    index: &Index,
    matcher: Option<&SparseMatcher>,
) -> BTreeSet<Vec<u8>> {
    let Some(matcher) = matcher else {
        return BTreeSet::new();
    };
    let mut expanded = BTreeSet::new();
    for entry in index
        .entries
        .iter()
        .filter(|entry| !entry.is_sparse_dir() && !matcher.includes_file(entry.path.as_bytes()))
    {
        let path = entry.path.as_bytes();
        let mut start = 0usize;
        while let Some(relative) = path[start..].iter().position(|byte| *byte == b'/') {
            let end = start + relative;
            let mut probe = path[..=end].to_vec();
            probe.extend_from_slice(b"__sley_sparse_probe__");
            if !matcher.includes_file(&probe) {
                expanded.insert(path[..=end].to_vec());
                break;
            }
            start = end + 1;
        }
    }
    expanded
}

fn expand_sparse_index_impl(
    index: &mut Index,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    emit_trace: bool,
) -> Result<bool> {
    if !index.entries.iter().any(IndexEntry::is_sparse_dir) {
        // Still strip a stray `sdir` marker so the written index is recorded full.
        let had_marker = index.is_sparse();
        index.clear_sparse_extension()?;
        if had_marker && emit_trace {
            sley_core::trace2::region("index", "ensure_full_index");
        }
        return Ok(had_marker);
    }
    let mut expanded: Vec<IndexEntry> = Vec::with_capacity(index.entries.len());
    for entry in std::mem::take(&mut index.entries) {
        if !entry.is_sparse_dir() {
            expanded.push(entry);
            continue;
        }
        // The sparse-dir path ends in `/`; its OID is the directory's tree.
        let dir = entry.path.as_bytes();
        let dir_prefix = dir; // includes the trailing slash
        for (rel, (mode, oid)) in sley_diff_merge::flatten_tree(db, format, &entry.oid)? {
            let mut full_path = dir_prefix.to_vec();
            full_path.extend_from_slice(&rel);
            let mut blob = blank_sparse_blob_entry(format, &full_path, mode, oid);
            // Re-collapsed entries are skip-worktree (they live outside the cone).
            blob.set_skip_worktree(true);
            expanded.push(blob);
        }
    }
    expanded.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    index.entries = expanded;
    index.clear_sparse_extension()?;
    normalize_index_version_for_extended_flags(index);
    if emit_trace {
        sley_core::trace2::region("index", "ensure_full_index");
    }
    Ok(true)
}

pub(crate) fn index_sparse_dir_contains_path(index: &Index, git_path: &[u8]) -> bool {
    index.entries.iter().any(|entry| {
        entry.is_sparse_dir()
            && git_path.starts_with(entry.path.as_bytes())
            && git_path.len() > entry.path.len()
    })
}

/// Builds a minimal index entry for an expanded sparse blob: zeroed stat fields
/// (the file is not in the worktree), the given mode/oid, and a fresh name
/// length. Stat fields are zero because a skip-worktree file has no on-disk
/// presence to record.
pub(crate) fn blank_sparse_blob_entry(
    format: ObjectFormat,
    path: &[u8],
    mode: u32,
    oid: ObjectId,
) -> IndexEntry {
    let _ = format;
    let mut entry = IndexEntry {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: 0,
        flags_extended: 0,
        path: path.into(),
    };
    entry.refresh_name_length();
    entry
}

/// Collapses fully-out-of-cone directories in `index` into single sparse-
/// directory entries (mode `040000`, skip-worktree, the directory tree's OID),
/// then marks the index with the `sdir` extension. A directory is collapsible
/// when *every* entry under it is skip-worktree and stage 0 — i.e. nothing in it
/// is in the cone or conflicted. The shallowest such directory subsumes deeper
/// ones, matching git's `convert_to_sparse` cache-tree walk.
pub(crate) fn collapse_to_sparse_index(
    index: &mut Index,
    matcher: &SparseMatcher,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<()> {
    // First expand any pre-existing sparse-dir entries so the collapse decision
    // sees a uniform full index (idempotent re-collapse).
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        expand_sparse_index(index, db, format)?;
    }

    // Any unmerged (stage != 0) entry forbids a sparse index entirely (the cache
    // tree cannot be built), so stay full — matching git's bail.
    if index.entries.iter().any(|e| index_entry_stage(e) != 0) {
        index.clear_sparse_extension()?;
        return Ok(());
    }

    index
        .entries
        .sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    // Determine, for every directory prefix, whether it contains any in-cone
    // path. A directory with no in-cone descendant is collapsible.
    use std::collections::BTreeMap;
    let mut dir_has_in_cone: BTreeMap<Vec<u8>, bool> = BTreeMap::new();
    for entry in &index.entries {
        let path = entry.path.as_bytes();
        // An explicitly materialized out-of-cone path (skip-worktree cleared
        // by update-index/add/mv) prevents its directory from collapsing just
        // as surely as a pattern-in-cone path. Re-collapsing it would discard
        // the user's per-leaf sparse override.
        let in_cone = matcher.includes_file(path)
            || !entry.is_skip_worktree()
            || sley_index::is_gitlink(entry.mode);
        let mut start = 0usize;
        while let Some(rel) = path
            .get(start..)
            .and_then(|s| s.iter().position(|b| *b == b'/'))
        {
            let end = start + rel;
            let dir = path[..end].to_vec();
            let flag = dir_has_in_cone.entry(dir).or_insert(false);
            *flag = *flag || in_cone;
            start = end + 1;
        }
    }

    // The collapsible directories are those with no in-cone descendant; keep only
    // the shallowest (a directory whose ancestor is also collapsible is subsumed).
    let collapsible: Vec<Vec<u8>> = {
        let all: Vec<Vec<u8>> = dir_has_in_cone
            .iter()
            .filter(|(_, has)| !**has)
            .map(|(dir, _)| dir.clone())
            .collect();
        all.iter()
            .filter(|dir| {
                !all.iter().any(|other| {
                    other != *dir
                        && dir
                            .strip_prefix(other.as_slice())
                            .is_some_and(|rest| rest.first() == Some(&b'/'))
                })
            })
            .cloned()
            .collect()
    };
    if collapsible.is_empty() {
        index.clear_sparse_extension()?;
        return Ok(());
    }

    let mut checker = db.presence_checker();
    let mut new_entries: Vec<IndexEntry> = Vec::with_capacity(index.entries.len());
    let mut consumed: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for dir in &collapsible {
        // Gather the entries that live strictly under this directory.
        let mut subtree: Vec<&IndexEntry> = index
            .entries
            .iter()
            .filter(|e| {
                e.path
                    .as_bytes()
                    .strip_prefix(dir.as_slice())
                    .is_some_and(|rest| rest.first() == Some(&b'/'))
            })
            .collect();
        if subtree.is_empty() {
            continue;
        }
        subtree.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        // Build the subtree object and capture its OID.
        let mut prefix = dir.clone();
        prefix.push(b'/');
        let tree_entries: Vec<WriteTreeEntry<'_>> = subtree
            .iter()
            .map(|e| WriteTreeEntry {
                path: e.path.as_bytes(),
                mode: e.mode,
                oid: e.oid.clone(),
            })
            .collect();
        let tree_oid =
            write_tree_entries_stream(&tree_entries, &prefix, None, db, &mut checker, false)?;
        // Mark every consumed path so the second pass drops them.
        for e in &subtree {
            consumed.insert(e.path.as_bytes().to_vec());
        }
        // The sparse-dir entry's name is the directory path WITH a trailing slash.
        let mut sparse_path = dir.clone();
        sparse_path.push(b'/');
        let mut sparse_entry =
            blank_sparse_blob_entry(format, &sparse_path, SPARSE_DIR_MODE, tree_oid);
        sparse_entry.set_skip_worktree(true);
        new_entries.push(sparse_entry);
    }
    // Carry forward every entry that was not collapsed.
    for entry in &index.entries {
        if consumed.contains(entry.path.as_bytes()) {
            continue;
        }
        new_entries.push(entry.clone());
    }
    new_entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    index.entries = new_entries;
    index.set_sparse_extension();
    normalize_index_version_for_extended_flags(index);
    sley_core::trace2::region("index", "convert_to_sparse");
    Ok(())
}

/// Whether the worktree file described by `metadata` is up to date with `entry`'s
/// cached index stat, using the size + mtime heuristic at the core of git's
/// `ie_match_stat`. A freshly-checked-out (clean) file matches; a file that was
/// deleted and later recreated — as happens when an out-of-cone path reappears in
/// the worktree — gets a fresh mtime and so reads as modified, which is exactly
/// the state git declines to overwrite during a sparse update.
pub(crate) fn worktree_entry_is_uptodate(entry: &IndexEntry, metadata: &fs::Metadata) -> bool {
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

/// Returns whether an out-of-cone worktree entry is safe to remove.
///
/// Expanding a collapsed sparse-directory entry creates leaf entries without
/// cached stat data. In that representation, the cheap stat comparison cannot
/// prove that a present file is clean even when its bytes exactly match the
/// index. Fall back to the normal worktree/index content comparison so sparse
/// representation changes do not manufacture local modifications.
fn sparse_checkout_worktree_entry_is_uptodate(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    entry: &IndexEntry,
    metadata: &fs::Metadata,
) -> Result<bool> {
    if worktree_entry_is_uptodate(entry, metadata) {
        return Ok(true);
    }
    let stat_cache_is_blank = entry.ctime_seconds == 0
        && entry.ctime_nanoseconds == 0
        && entry.mtime_seconds == 0
        && entry.mtime_nanoseconds == 0
        && entry.dev == 0
        && entry.ino == 0
        && entry.uid == 0
        && entry.gid == 0
        && entry.size == 0;
    if !stat_cache_is_blank {
        return Ok(false);
    }
    let Some(worktree_entry) = worktree_entry_for_git_path(
        worktree_root,
        git_dir,
        format,
        entry.path.as_bytes(),
        &entry.oid,
        entry.mode,
        None,
    )?
    else {
        return Ok(false);
    };
    Ok(worktree_entry.mode == entry.mode && worktree_entry.oid == entry.oid)
}

pub(crate) fn worktree_entry_ref_is_uptodate(
    entry: &IndexEntryRef<'_>,
    metadata: &fs::Metadata,
) -> bool {
    if u64::from(entry.size) != metadata.len() {
        return false;
    }
    let Some((mtime_seconds, mtime_nanoseconds)) = file_mtime_parts(metadata) else {
        return false;
    };
    u64::from(entry.mtime_seconds) == mtime_seconds
        && u64::from(entry.mtime_nanoseconds) == mtime_nanoseconds
}

/// The file's modification time split into whole seconds and the sub-second
/// nanosecond remainder, matching how git stores `mtime` in the index.
pub(crate) fn file_mtime_parts(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((duration.as_secs(), u64::from(duration.subsec_nanos())))
}

/// Write a git metadata file through a sibling `.lock` file and atomic rename.
///
/// This helper is intended for small repository/worktree metadata files such as
/// `HEAD`, `config.worktree`, or state files under `.git/`. It deliberately does
/// not try to replace object or pack writers, which have their own durability
/// and naming rules.
pub fn write_metadata_file_atomic(
    path: impl AsRef<Path>,
    bytes: &[u8],
    options: AtomicMetadataWriteOptions,
) -> Result<AtomicMetadataWriteResult> {
    let path = path.as_ref();
    let parent = path.parent().ok_or_else(|| {
        GitError::InvalidPath(format!("metadata path has no parent: {}", path.display()))
    })?;
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = metadata_lock_path(path)?;
    let mut lock = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(GitError::Transaction(format!(
                "metadata lock already exists: {}",
                lock_path.display()
            )));
        }
        Err(err) => return Err(err.into()),
    };
    if let Err(err) = lock.write_all(bytes) {
        let _ = fs::remove_file(&lock_path);
        return Err(err.into());
    }
    if options.fsync_file
        && let Err(err) = lock.sync_all()
    {
        let _ = fs::remove_file(&lock_path);
        return Err(err.into());
    }
    drop(lock);
    if let Err(err) = fs::rename(&lock_path, path) {
        let _ = fs::remove_file(&lock_path);
        return Err(err.into());
    }
    if options.fsync_dir
        && let Ok(dir) = fs::File::open(parent)
    {
        dir.sync_all()?;
    }
    let metadata = fs::metadata(path)?;
    Ok(AtomicMetadataWriteResult {
        path: path.to_path_buf(),
        len: metadata.len(),
        mtime: file_mtime_parts(&metadata),
    })
}

pub(crate) fn metadata_lock_path(path: &Path) -> Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        GitError::InvalidPath(format!("metadata path has no filename: {}", path.display()))
    })?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(path.with_file_name(lock_name))
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
        None,
        None,
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

pub(crate) fn materialize_index_entry_file(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    file_path: &Path,
    entry: &IndexEntry,
) -> Result<()> {
    // A gitlink (mode 160000) has no blob in this object store and materializes
    // as a directory (git's `write_entry` S_IFGITLINK arm: mkdir, never read an
    // object). Single gitlink rule via `sley_index::is_gitlink`; without it a
    // sparse re-materialization of a submodule path would fail with "not found:
    // blob object <commit-oid>".
    if sley_index::is_gitlink(entry.mode) {
        materialize_gitlink_dir(worktree_root, file_path)?;
        return Ok(());
    }
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    prepare_blob_parent_dirs(worktree_root, file_path)?;
    remove_existing_worktree_path(file_path)?;
    write_blob_body_or_symlink(file_path, entry.mode, &object.body, &object.body)?;
    Ok(())
}

pub(crate) fn set_skip_worktree(entry: &mut IndexEntry) {
    entry.flags |= INDEX_FLAG_EXTENDED;
    entry.flags_extended |= INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
}

pub(crate) fn clear_skip_worktree(entry: &mut IndexEntry) {
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
    restore_worktree_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &head_entries,
        paths,
    )
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
    restore_worktree_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
        paths,
    )
}

pub(crate) fn restore_worktree_paths_from_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
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
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    let mut restored = BTreeSet::new();
    let matched_paths = checkout_selected_paths(
        worktree_root,
        paths,
        index_entries
            .iter()
            .chain(source_entries.keys())
            .map(Vec::as_slice),
        false,
    )?;
    for path in matched_paths {
        if let Some(entry) = source_entries.get(&path) {
            materialize_path_restore_entry_filtered(
                db,
                format,
                worktree_root,
                git_dir,
                &path,
                entry,
                &config,
            )?;
        } else {
            remove_worktree_file(worktree_root, &path)?;
        }
        restored.insert(path);
    }
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

#[cfg(test)]
mod sparse_index_expansion_tests {
    use super::*;

    #[test]
    fn mixed_sparse_index_roundtrip_preserves_expanded_boundary() {
        let oid = ObjectId::from_raw(ObjectFormat::Sha1, &[3; 20]).expect("object id");
        let mut collapsed =
            blank_sparse_blob_entry(ObjectFormat::Sha1, b"outside/", SPARSE_DIR_MODE, oid);
        collapsed.set_skip_worktree(true);
        let mut expanded = blank_sparse_blob_entry(ObjectFormat::Sha1, b"folder2/a", 0o100644, oid);
        expanded.set_skip_worktree(true);
        let mut index = Index {
            version: 3,
            entries: vec![expanded, collapsed],
            extensions: Vec::new(),
            checksum: None,
        };
        index.entries.sort_by(compare_index_key);
        index.set_sparse_extension();
        let sparse = SparseCheckout {
            patterns: vec![b"/*".to_vec(), b"!/*/".to_vec(), b"/deep/".to_vec()],
            sparse_index: true,
        };
        let matcher = SparseMatcher::new(&sparse, SparseCheckoutMode::Cone);

        let encoded = index.write(ObjectFormat::Sha1).expect("serialize index");
        let decoded = Index::parse(&encoded, ObjectFormat::Sha1).expect("parse index");

        assert!(decoded.entries.iter().any(|entry| entry.is_sparse_dir()));
        assert!(
            decoded
                .entries
                .iter()
                .any(|entry| entry.path.as_bytes() == b"folder2/a")
        );
        assert_eq!(
            sparse_index_expanded_boundaries(&decoded, Some(&matcher)),
            BTreeSet::from([b"folder2/".to_vec()])
        );
    }

    #[test]
    fn selective_expansion_leaves_unmatched_sparse_directories_collapsed() {
        let root = tempfile::tempdir().expect("temporary repository");
        let git_dir = root.path().join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("object directory");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"payload\n".to_vec()))
            .expect("write blob");
        let tree = db
            .write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree {
                    entries: vec![TreeEntry {
                        mode: 0o100644,
                        name: BString::from("file"),
                        oid: blob,
                    }],
                }
                .write(),
            ))
            .expect("write tree");
        let mut sparse =
            blank_sparse_blob_entry(ObjectFormat::Sha1, b"outside/", SPARSE_DIR_MODE, tree);
        sparse.set_skip_worktree(true);
        let mut index = Index {
            version: 3,
            entries: vec![sparse],
            extensions: Vec::new(),
            checksum: None,
        };
        index.set_sparse_extension();

        assert!(
            !expand_sparse_index_directories(&mut index, &db, ObjectFormat::Sha1, |_| false,)
                .expect("leave unrelated directory collapsed")
        );
        assert!(index.entries[0].is_sparse_dir());
        assert!(index.is_sparse());

        assert!(
            expand_sparse_index_directories(&mut index, &db, ObjectFormat::Sha1, |path| path
                == b"outside/",)
            .expect("expand selected directory")
        );
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].path.as_bytes(), b"outside/file");
        assert!(index.entries[0].is_skip_worktree());
        assert!(!index.is_sparse());
    }

    #[test]
    fn collapse_preserves_materialized_out_of_cone_leaf() {
        let root = tempfile::tempdir().expect("temporary repository");
        let git_dir = root.path().join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("object directory");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"payload\n".to_vec()))
            .expect("write blob");
        let entry = blank_sparse_blob_entry(ObjectFormat::Sha1, b"outside/file", 0o100644, blob);
        assert!(!entry.is_skip_worktree());
        let mut index = Index {
            version: 3,
            entries: vec![entry],
            extensions: Vec::new(),
            checksum: None,
        };
        let sparse = SparseCheckout {
            patterns: vec![b"/*".to_vec(), b"!/*/".to_vec(), b"/deep/".to_vec()],
            sparse_index: true,
        };
        let matcher = SparseMatcher::new(&sparse, SparseCheckoutMode::Cone);

        collapse_to_sparse_index(&mut index, &matcher, &db, ObjectFormat::Sha1)
            .expect("collapse sparse index");

        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].path.as_bytes(), b"outside/file");
        assert!(!index.entries[0].is_sparse_dir());
        assert!(!index.is_sparse());
    }

    #[test]
    fn collapse_preserves_out_of_cone_gitlink_leaf() {
        let root = tempfile::tempdir().expect("temporary repository");
        let git_dir = root.path().join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("object directory");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let commit = ObjectId::from_raw(ObjectFormat::Sha1, &[7; 20]).expect("gitlink oid");
        let mut entry =
            blank_sparse_blob_entry(ObjectFormat::Sha1, b"outside/module", 0o160000, commit);
        entry.set_skip_worktree(true);
        let mut index = Index {
            version: 3,
            entries: vec![entry],
            extensions: Vec::new(),
            checksum: None,
        };
        let sparse = SparseCheckout {
            patterns: vec![b"/*".to_vec(), b"!/*/".to_vec(), b"/deep/".to_vec()],
            sparse_index: true,
        };
        let matcher = SparseMatcher::new(&sparse, SparseCheckoutMode::Cone);

        collapse_to_sparse_index(&mut index, &matcher, &db, ObjectFormat::Sha1)
            .expect("preserve gitlink in full index");

        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].path.as_bytes(), b"outside/module");
        assert!(sley_index::is_gitlink(index.entries[0].mode));
        assert!(!index.entries[0].is_sparse_dir());
        assert!(!index.is_sparse());
    }
}

#[cfg(all(test, unix))]
mod checkout_parent_safety_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn collision_probe_preserves_component_boundaries_and_filesystem_identity() {
        let root = tempfile::tempdir().expect("temporary worktree");
        let mut probe = CheckoutCollisionProbe::new(root.path()).expect("collision probe");
        let parent = probe.key(b"dir").expect("parent collision key");
        let child = probe.key(b"dir/file").expect("child collision key");
        assert!(checkout_filesystem_paths_collide(&parent, &child));

        fs::create_dir(root.path().join("ä")).expect("unicode probe directory");
        if root.path().join("a\u{308}").exists() {
            let decomposed = probe
                .key("a\u{308}/file".as_bytes())
                .expect("decomposed collision key");
            let composed = probe.key("ä".as_bytes()).expect("composed collision key");
            assert!(checkout_filesystem_paths_collide(&decomposed, &composed));
        }
    }

    #[test]
    fn preparing_blob_parent_replaces_symlink_instead_of_following_it() {
        let root = tempfile::tempdir().expect("temporary worktree");
        let outside = tempfile::tempdir().expect("outside directory");
        symlink(outside.path(), root.path().join("D")).expect("leading symlink");

        prepare_blob_parent_dirs(root.path(), &root.path().join("D/file"))
            .expect("prepare real parent directory");

        let metadata = fs::symlink_metadata(root.path().join("D")).expect("D metadata");
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
        assert!(!outside.path().join("file").exists());
    }

    #[test]
    fn parallel_materializer_uses_queue_and_serializes_shared_prefix() {
        let root = tempfile::tempdir().expect("temporary worktree");
        let outside = tempfile::tempdir().expect("outside directory");
        symlink(outside.path(), root.path().join("D")).expect("leading symlink");
        let config =
            GitConfig::parse(b"[checkout]\n\tworkers = 2\n\tthresholdForParallelism = 0\n")
                .expect("parallel checkout config");
        let oid = ObjectId::null(ObjectFormat::Sha1);
        let prepared = [b"D/A".as_slice(), b"D/B".as_slice()]
            .into_iter()
            .map(|path| PreparedCheckoutEntry {
                path: path.to_vec(),
                entry: TrackedEntry {
                    mode: 0o100644,
                    oid,
                },
                body: Some(path.to_vec()),
                index_template: None,
            })
            .collect();

        let entries = materialize_prepared_checkout_entries(root.path(), &config, prepared)
            .expect("parallel materialization");

        assert_eq!(entries.len(), 2);
        assert_eq!(fs::read(root.path().join("D/A")).expect("D/A"), b"D/A");
        assert_eq!(fs::read(root.path().join("D/B")).expect("D/B"), b"D/B");
        assert!(!outside.path().join("A").exists());
        assert!(!outside.path().join("B").exists());
    }
}
