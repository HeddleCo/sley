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
    let mut dirty = false;
    if smudge_config.is_some() {
        dirty = !modified_index_entries(worktree_root, git_dir, format)?.is_empty();
    } else {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
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
    let mut materialized_paths: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut collided_paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut index_entries = Vec::new();
    let mut delayed_checkout = DelayedCheckoutQueue::default();
    for (path, entry) in &target_entries {
        if ignore_case {
            let folded = checkout_collision_key(path);
            if let Some((_, existing_path)) = materialized_paths
                .iter()
                .find(|(existing, _)| checkout_paths_collide(existing, &folded))
            {
                collided_paths.insert(existing_path.clone());
                collided_paths.insert(path.clone());
                index_entries.push(unmaterialized_index_entry(path, entry));
                continue;
            }
            materialized_paths.push((folded, path.clone()));
        }
        // Single type-by-mode materializer: gitlinks become a directory (mkdir,
        // no blob read), symlinks (mode 120000) a real symlink to the raw blob
        // bytes, and regular files the smudge-filtered content. Inlining the blob
        // write here previously dropped the symlink arm and wrote the link target
        // as a regular file — the whole symlink-checkout class.
        index_entries.push(materialize_tree_entry_with_optional_smudge(
            &db,
            format,
            worktree_root,
            path,
            entry,
            smudge_config,
            attributes.as_ref(),
            Some(&mut delayed_checkout),
        )?);
    }
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

fn checkout_collision_key(path: &[u8]) -> Vec<u8> {
    path.iter().map(u8::to_ascii_lowercase).collect()
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
    mut delayed: DelayedCheckoutQueue,
) -> Result<BTreeMap<Vec<u8>, IndexEntry>> {
    if delayed.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut updates = BTreeMap::new();
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
                        let index_entry = write_delayed_checkout_output(
                            worktree_root,
                            &path,
                            &delayed_entry.entry,
                            &output,
                        )?;
                        if let Some(index_entry) = index_entry {
                            updates.insert(path, index_entry);
                        }
                    }
                    Ok(ProcessFilterOutcome::Unsupported) => {
                        eprintln!("error: external filter '{}' failed", process);
                        had_error = true;
                        keep_filter = false;
                    }
                    Ok(ProcessFilterOutcome::Status(status)) => {
                        eprintln!(
                            "error: external filter '{}' returned status {status}",
                            process
                        );
                        had_error = true;
                        keep_filter = false;
                    }
                    Err(err) => {
                        if err.protocol {
                            eprintln!("error: external filter '{}' failed", process);
                        }
                        had_error = true;
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
    }

    if had_error {
        return Err(GitError::Exit(1));
    }
    Ok(updates)
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
    mut delayed: Option<&mut DelayedCheckoutQueue>,
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
    let config = smudge_config.expect("checked above");
    let matcher = attributes.expect("attributes are built when smudge_config is set");
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
            let queue = delayed
                .as_deref_mut()
                .expect("delay is only reported when a queue is available");
            queue.enqueue(process, path, entry);
            return Ok(unmaterialized_index_entry(path, entry));
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
pub(crate) fn checkout_commit_to_index_and_worktree_sparse(
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
    let mut dirty = false;
    stream_short_status(worktree_root, git_dir, format, |entry| {
        if previously_skipped.contains(entry.path) {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
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
                    &db,
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
                    &db,
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
            if let Some(updated) = restore_index_entry_maybe_delayed(
                worktree_root,
                git_dir,
                format,
                &db,
                &index.entries[position],
                options.smudge_config,
                Some(&stat_cache),
                Some(&mut delayed_checkout),
            )? {
                refreshed.insert(position, updated);
                restored.insert(path);
            }
        }
    }

    let mut delayed_updates = finish_delayed_checkout(worktree_root, delayed_checkout)?;
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
    Ok(RestoreResult {
        restored: restored.len(),
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
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &head_entries,
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
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
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
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
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
    paths: &[PathBuf],
    allow_unmatched: bool,
) -> Result<RestoreResult> {
    let sparse = active_sparse_checkout(git_dir)?;
    if index.is_sparse() {
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
    let matched_paths = checkout_selected_paths(
        worktree_root,
        paths,
        index_entries
            .keys()
            .chain(source_entries.keys())
            .map(Vec::as_slice),
        allow_unmatched,
    )?;
    for path in matched_paths {
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
        let matcher = SparseMatcher::new(&sparse, mode);
        collapse_to_sparse_index(&mut index, &matcher, db, format)?;
    }
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(RestoreResult {
        restored: restored.len(),
    })
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
    index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: &[PathBuf],
    overlay: bool,
) -> Result<RestoreResult> {
    let index_version = index.version;
    let extensions = index_extensions_without_cache_tree(&index.extensions);
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    let mut index_entries = index
        .entries
        .into_iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut restored = BTreeSet::new();
    let matched_paths = checkout_selected_paths(
        worktree_root,
        paths,
        index_entries
            .keys()
            .chain(source_entries.keys())
            .map(Vec::as_slice),
        false,
    )?;
    for path in matched_paths {
        if let Some(entry) = source_entries.get(&path) {
            index_entries.insert(
                path.clone(),
                materialize_path_restore_entry_filtered(
                    db,
                    format,
                    worktree_root,
                    git_dir,
                    &path,
                    entry,
                    &config,
                )?,
            );
        } else if overlay {
            // Overlay mode (git checkout default): a path that matches the
            // pathspec but is absent from the source tree is left untouched
            // in both the index and the working tree.
            continue;
        } else {
            // No-overlay mode (git restore default, checkout --no-overlay):
            // drop the path from the index and the working tree.
            index_entries.remove(&path);
            remove_worktree_file(worktree_root, &path)?;
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
    refuse_if_current_working_directory_becomes_file(worktree_root, &target_entries)?;
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
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
    let mut delayed_checkout = DelayedCheckoutQueue::default();
    for (path, entry) in &target_entries {
        index_entries.push(materialize_tree_entry_with_optional_smudge(
            &db,
            format,
            worktree_root,
            path,
            entry,
            Some(&config),
            Some(&attributes),
            Some(&mut delayed_checkout),
        )?);
    }
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
    if fs::symlink_metadata(dir_path).is_ok_and(|metadata| !metadata.is_dir()) {
        remove_existing_worktree_path(dir_path)?;
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
    // Fast path: parent already a directory (the overwhelmingly common case).
    if parent.is_dir() {
        return Ok(());
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
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
    // git's `reset --mixed` preserves the skip-worktree bit on entries that survive
    // the reset (t7102 "--mixed preserves skip-worktree"): carry it forward from the
    // pre-reset index keyed by path, so reconstructed entries keep CE_SKIP_WORKTREE.
    let index_path = repository_index_path(git_dir);
    let prior_skip_worktree: BTreeSet<Vec<u8>> = match fs::read(&index_path) {
        Ok(bytes) => Index::parse(&bytes, format)?
            .entries
            .iter()
            .filter(|entry| entry.is_skip_worktree())
            .map(|entry| entry.path.as_bytes().to_vec())
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(err) => return Err(err.into()),
    };
    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        let mut restored = restored_head_index_entry(worktree_root, &db, path, entry)?;
        if prior_skip_worktree.contains(path) {
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
    index.upgrade_version_for_flags();
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

pub(crate) fn active_sparse_checkout(
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
        });
    };
    let matcher = SparseMatcher::new(sparse, mode);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    // Expand any collapsed sparse-directory entries to a full index before we
    // reconcile per-path: the apply loop reasons about individual blob paths, so
    // it must never see a sparse-dir entry. (Re-collapse happens at the end when
    // a sparse index is requested.)
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        expand_sparse_index(&mut index, &db, format)?;
    }
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
    })
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
    if !index.entries.iter().any(IndexEntry::is_sparse_dir) {
        // Still strip a stray `sdir` marker so the written index is recorded full.
        let had_marker = index.is_sparse();
        index.clear_sparse_extension()?;
        if had_marker {
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
    sley_core::trace2::region("index", "ensure_full_index");
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
        let in_cone = matcher.includes_file(path);
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
