//! Merge, rebase, pull, cherry-pick, revert, and merge-base commands.

use crate::commands::remote_cmds::{
    StdoutProgress, fetch_bundle, fetch_source_is_ssh, fetch_ssh_repository, ls_remote_git_dir,
};
use crate::*;
use sley_remote::FetchOptions;

// ===== git merge (3-way) =====

pub(crate) type MergeTreeMap = BTreeMap<Vec<u8>, (u32, ObjectId)>;

pub(crate) fn merge_read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(object.body.clone())
}

pub(crate) fn merge_is_regular_file(mode: u32) -> bool {
    mode == 0o100644 || mode == 0o100755
}

pub(crate) fn merge_index_entry(path: &[u8], mode: u32, oid: ObjectId, stage: u16) -> IndexEntry {
    let flags = ((stage & 0x3) << 12) | (path.len().min(0x0fff) as u16);
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
        flags,
        flags_extended: 0,
        path: BString::from(path),
    }
}

pub(crate) fn merge_write_worktree_file(
    worktree_root: &Path,
    path: &[u8],
    content: &[u8],
    mode: u32,
) -> Result<()> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&full, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(if mode == 0o100755 { 0o755 } else { 0o644 });
        fs::set_permissions(&full, perms)?;
    }
    let _ = mode;
    Ok(())
}

pub(crate) fn merge_remove_worktree_file(worktree_root: &Path, path: &[u8]) -> Result<()> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    if full.exists() {
        fs::remove_file(&full)?;
    }
    Ok(())
}

/// Per-path outcome of a 3-way tree merge.
pub(crate) enum MergePathResult {
    /// Cleanly resolved; `None` means the path is deleted in the result.
    Resolved(Option<(u32, ObjectId)>),
    /// Conflicted: carries the (mode, oid) for each present stage and the bytes
    /// (with conflict markers) plus mode to materialize in the worktree.
    Conflict {
        base: Option<(u32, ObjectId)>,
        ours: Option<(u32, ObjectId)>,
        theirs: Option<(u32, ObjectId)>,
        worktree: Option<(u32, Vec<u8>)>,
    },
}

type MergePathResults = BTreeMap<Vec<u8>, MergePathResult>;
type MergeConflictPaths = Vec<Vec<u8>>;

/// 3-way merge of three flattened trees. Writes any cleanly-merged blob content
/// to the ODB and returns per-path results plus the sorted list of conflicted paths.
pub(crate) fn three_way_merge_trees(
    db: &mut FileObjectDatabase,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
) -> Result<(MergePathResults, MergeConflictPaths)> {
    let mut all_paths = BTreeSet::new();
    all_paths.extend(base.keys().cloned());
    all_paths.extend(ours.keys().cloned());
    all_paths.extend(theirs.keys().cloned());

    let mut results = BTreeMap::new();
    let mut conflicts = Vec::new();
    for path in all_paths {
        let b = base.get(&path).cloned();
        let o = ours.get(&path).cloned();
        let t = theirs.get(&path).cloned();

        if o == t {
            results.insert(path, MergePathResult::Resolved(o));
            continue;
        }
        if o == b {
            results.insert(path, MergePathResult::Resolved(t));
            continue;
        }
        if t == b {
            results.insert(path, MergePathResult::Resolved(o));
            continue;
        }

        // Both sides changed differently relative to the base.
        let content_mergeable = matches!(&o, Some((mode, _)) if merge_is_regular_file(*mode))
            && matches!(&t, Some((mode, _)) if merge_is_regular_file(*mode))
            && match &b {
                Some((mode, _)) => merge_is_regular_file(*mode),
                None => true,
            };

        if let (true, Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) =
            (content_mergeable, &o, &t)
        {
            let base_bytes = match &b {
                Some((_, oid)) => merge_read_blob(db, oid)?,
                None => Vec::new(),
            };
            let ours_bytes = merge_read_blob(db, ours_oid)?;
            let theirs_bytes = merge_read_blob(db, theirs_oid)?;
            let merged = sley_diff_merge::merge_blobs(
                &base_bytes,
                &ours_bytes,
                &theirs_bytes,
                &sley_diff_merge::MergeBlobOptions {
                    ours_label,
                    theirs_label,
                    base_label: "merged common ancestors",
                    style: sley_diff_merge::ConflictStyle::Merge,
                },
            );
            if !merged.conflicted && ours_mode == theirs_mode {
                let oid = db.write_object(EncodedObject::new(ObjectType::Blob, merged.content))?;
                results.insert(path, MergePathResult::Resolved(Some((*ours_mode, oid))));
            } else {
                conflicts.push(path.clone());
                let worktree_mode = if *ours_mode == *theirs_mode {
                    *ours_mode
                } else {
                    0o100644
                };
                results.insert(
                    path,
                    MergePathResult::Conflict {
                        base: b,
                        ours: o.clone(),
                        theirs: t.clone(),
                        worktree: Some((worktree_mode, merged.content)),
                    },
                );
            }
        } else {
            // Non-content-mergeable: modify/delete, add/add of non-files, type or
            // mode changes. Keep the surviving side's bytes in the worktree.
            conflicts.push(path.clone());
            let worktree = if let Some((mode, oid)) = o.as_ref().or(t.as_ref()) {
                Some((*mode, merge_read_blob(db, oid)?))
            } else {
                None
            };
            results.insert(
                path,
                MergePathResult::Conflict {
                    base: b,
                    ours: o,
                    theirs: t,
                    worktree,
                },
            );
        }
    }
    Ok((results, conflicts))
}

fn write_merge_result_diffstat(
    stdout: &mut io::Stdout,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
) -> Result<()> {
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    write_diff_stat(
        stdout,
        &entries,
        db,
        None,
        false,
        DiffStatOptions {
            compact_summary: false,
            stat_count: None,
            color: false,
        },
    )
}

/// Create a merge commit with two parents and advance the current branch (or
/// detached HEAD) to it, writing a reflog entry.
fn merge_commit_and_advance(
    git_dir: &Path,
    refs: &FileRefStore,
    format: ObjectFormat,
    head_oid: &ObjectId,
    other_oid: &ObjectId,
    tree: ObjectId,
    message: Vec<u8>,
) -> Result<ObjectId> {
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![*head_oid, *other_oid],
            author,
            committer: committer.clone(),
            message,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: Some(RefTarget::Direct(*head_oid)),
        new: RefTarget::Direct(oid),
        reflog: Some(ReflogEntry {
            old_oid: *head_oid,
            new_oid: oid,
            committer,
            message: format!("merge {other_oid}: Merge made by the 'ort' strategy.").into_bytes(),
        }),
    });
    tx.commit()?;
    Ok(oid)
}

#[derive(Default)]
struct MergeOptions {
    message: Option<String>,
    no_ff: bool,
    ff_only: bool,
    no_commit: bool,
    quiet: bool,
}

pub(crate) fn cmd_merge(args: &[String]) -> Result<()> {
    let mut options = MergeOptions::default();
    let mut abort = false;
    let mut continue_merge = false;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--abort" => abort = true,
            "--continue" => continue_merge = true,
            "--no-ff" => options.no_ff = true,
            "--ff" => options.no_ff = false,
            "--ff-only" => options.ff_only = true,
            "--no-commit" => options.no_commit = true,
            "--commit" => options.no_commit = false,
            "-q" | "--quiet" => options.quiet = true,
            "--no-quiet" => options.quiet = false,
            "-m" | "--message" => {
                options.message = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("merge -m requires a value".into()))?
                        .clone(),
                );
            }
            value if value.starts_with("--message=") => {
                options.message = value
                    .strip_prefix("--message=")
                    .map(|value| value.to_string());
            }
            "--" => {
                positional.extend(iter.by_ref().map(|value| value.to_string()));
                break;
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported merge option {value}"
                )));
            }
            value => positional.push(value.to_string()),
        }
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let refs = FileRefStore::new(&git_dir, format);

    if abort {
        if !positional.is_empty() {
            eprintln!("fatal: --abort expects no arguments");
            return Err(GitError::Exit(129));
        }
        return cmd_merge_abort();
    }
    if continue_merge {
        if !positional.is_empty() || options.no_ff || options.ff_only || options.message.is_some() {
            eprintln!("fatal: --continue expects no arguments");
            return Err(GitError::Exit(129));
        }
        return cmd_merge_continue();
    }

    if git_dir.join("MERGE_HEAD").exists() {
        return Err(GitError::Command(
            "You have not concluded your merge (MERGE_HEAD exists).".into(),
        ));
    }

    let target = match positional.as_slice() {
        [target] => target.clone(),
        [] => {
            return Err(GitError::Command("merge requires a commit argument".into()));
        }
        _ => {
            return Err(GitError::Unsupported(
                "octopus merges (multiple commits) are not supported yet".into(),
            ));
        }
    };

    let other_oid = if target == "FETCH_HEAD" {
        resolve_fetch_head_revision(&git_dir, format)?
    } else {
        resolve_revision(&git_dir, format, &target)?
    };
    let head_oid = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
        Some(RefTarget::Direct(oid)) => Some(oid),
        None => None,
    };

    // Unborn HEAD: behave like a checkout of the other commit.
    let Some(head_oid) = head_oid else {
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: None,
            new: RefTarget::Direct(other_oid),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(format)?,
                new_oid: other_oid,
                committer: commit_identity_from_env("COMMITTER")?,
                message: format!("merge {target}: Fast-forward").into_bytes(),
            }),
        });
        tx.commit()?;
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &other_oid,
        )?;
        return Ok(());
    };

    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let bases = merge_bases(&db, format, &head_oid, &other_oid)?;

    // Already up to date: other is reachable from HEAD.
    if other_oid == head_oid || bases.iter().any(|base| base == &other_oid) {
        if !options.quiet {
            println!("Already up to date.");
        }
        return Ok(());
    }

    // Fast-forward: HEAD is an ancestor of other.
    let can_fast_forward = bases.iter().any(|base| base == &head_oid);
    if can_fast_forward && !options.no_ff {
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: Some(RefTarget::Direct(head_oid)),
            new: RefTarget::Direct(other_oid),
            reflog: Some(ReflogEntry {
                old_oid: head_oid,
                new_oid: other_oid,
                committer: commit_identity_from_env("COMMITTER")?,
                message: format!("merge {target}: Fast-forward").into_bytes(),
            }),
        });
        tx.commit()?;
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &other_oid,
        )?;
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "Updating {}..{}",
                format_log_abbrev_oid(&head_oid),
                format_log_abbrev_oid(&other_oid)
            )?;
            writeln!(stdout, "Fast-forward")?;
            let head_tree = commit_tree_oid(&db, format, &head_oid)?;
            let other_tree = commit_tree_oid(&db, format, &other_oid)?;
            write_merge_result_diffstat(&mut stdout, &db, format, &head_tree, &other_tree)?;
            stdout.flush()?;
        }
        return Ok(());
    }

    if options.ff_only {
        eprintln!("fatal: Not possible to fast-forward, aborting.");
        return Err(GitError::Exit(128));
    }

    // True 3-way merge.
    let base_oid = bases.first().cloned();
    if base_oid.is_none() {
        eprintln!("fatal: refusing to merge unrelated histories");
        return Err(GitError::Exit(128));
    }
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let other_tree = commit_tree_oid(&db, format, &other_oid)?;
    let base_tree = match &base_oid {
        Some(oid) => Some(commit_tree_oid(&db, format, oid)?),
        None => None,
    };
    let ours_map = stash_tree_entry_map(&db, format, &head_tree)?;
    let theirs_map = stash_tree_entry_map(&db, format, &other_tree)?;
    let base_map = match &base_tree {
        Some(tree) => stash_tree_entry_map(&db, format, tree)?,
        None => MergeTreeMap::new(),
    };

    let ours_label = "HEAD".to_string();
    let theirs_label = target.clone();
    let mut write_db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let (results, conflicts) = three_way_merge_trees(
        &mut write_db,
        &base_map,
        &ours_map,
        &theirs_map,
        &ours_label,
        &theirs_label,
    )?;

    let target_is_branch = match branch_ref_name(&target) {
        Ok(name) => refs.read_ref(&name)?.is_some(),
        Err(_) => false,
    };
    let default_message = if target == "FETCH_HEAD" {
        fetch_head_merge_record(&git_dir, format)
            .map(|record| format!("Merge {}", record.description))
            .unwrap_or_else(|_| format!("Merge commit '{target}'"))
    } else if target_is_branch {
        format!("Merge branch '{target}'")
    } else {
        format!("Merge commit '{target}'")
    };
    let message = options.message.clone().unwrap_or(default_message);

    if conflicts.is_empty() {
        // Build the merged tree via a temporary stage-0 index, then commit + sync.
        let mut entries = Vec::new();
        for (path, result) in &results {
            if let MergePathResult::Resolved(Some((mode, oid))) = result {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let index = Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            sley_worktree::repository_index_path(&git_dir),
            index.write(format)?,
        )?;
        let merged_tree = sley_worktree::write_tree_from_index(&git_dir, format)?;

        if options.no_commit {
            fs::write(git_dir.join("MERGE_HEAD"), format!("{other_oid}\n"))?;
            fs::write(git_dir.join("MERGE_MSG"), format!("{message}\n"))?;
            for (path, result) in &results {
                if let MergePathResult::Resolved(value) = result {
                    match value {
                        Some((mode, oid)) => {
                            let content = merge_read_blob(&db, oid)?;
                            merge_write_worktree_file(&worktree_root, path, &content, *mode)?;
                        }
                        None => merge_remove_worktree_file(&worktree_root, path)?,
                    }
                }
            }
            if !options.quiet {
                println!("Automatic merge went well; stopped before committing as requested");
            }
            return Ok(());
        }

        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(stdout, "Merge made by the 'ort' strategy.")?;
            write_merge_result_diffstat(&mut stdout, &db, format, &head_tree, &merged_tree)?;
            stdout.flush()?;
        }
        let merged_oid = merge_commit_and_advance(
            &git_dir,
            &refs,
            format,
            &head_oid,
            &other_oid,
            merged_tree,
            commit_cleanup_message(message.clone().into_bytes(), CommitCleanupMode::Whitespace),
        )?;
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &merged_oid,
        )?;
        return Ok(());
    }

    // Conflicted merge: write a staged index, materialize worktree, record state.
    let mut entries = Vec::new();
    for (path, result) in &results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict {
                base, ours, theirs, ..
            } => {
                if let Some((mode, oid)) = base {
                    entries.push(merge_index_entry(path, *mode, *oid, 1));
                }
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 2));
                }
                if let Some((mode, oid)) = theirs {
                    entries.push(merge_index_entry(path, *mode, *oid, 3));
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags >> 12).cmp(&(right.flags >> 12)))
    });
    let index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(
        sley_worktree::repository_index_path(&git_dir),
        index.write(format)?,
    )?;

    // Materialize merged/conflicted content into the worktree.
    for (path, result) in &results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = merge_read_blob(&db, oid)?;
                    merge_write_worktree_file(&worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => merge_remove_worktree_file(&worktree_root, path)?,
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    merge_write_worktree_file(&worktree_root, path, content, *mode)?
                }
                None => merge_remove_worktree_file(&worktree_root, path)?,
            },
        }
    }

    fs::write(git_dir.join("MERGE_HEAD"), format!("{other_oid}\n"))?;
    let mut merge_msg = format!("{message}\n\n# Conflicts:\n");
    for path in &conflicts {
        merge_msg.push_str(&format!("#\t{}\n", String::from_utf8_lossy(path)));
    }
    fs::write(git_dir.join("MERGE_MSG"), merge_msg)?;
    fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;

    for path in &conflicts {
        println!("Auto-merging {}", String::from_utf8_lossy(path));
        println!(
            "CONFLICT (content): Merge conflict in {}",
            String::from_utf8_lossy(path)
        );
    }
    eprintln!("Automatic merge failed; fix conflicts and then commit the result.");
    Err(GitError::Exit(1))
}

// ===== pull / rebase / merge-continue =====
pub(crate) fn cmd_merge_abort() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.is_file() {
        eprintln!("fatal: There is no merge to abort (MERGE_HEAD missing).");
        return Err(GitError::Exit(128));
    }

    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let orig_head_path = git_dir.join("ORIG_HEAD");
    let target_oid = if orig_head_path.is_file() {
        let contents = fs::read_to_string(&orig_head_path)?;
        ObjectId::from_hex(format, contents.trim()).map_err(|_| {
            GitError::InvalidObject(format!("invalid ORIG_HEAD value {}", contents.trim()))
        })?
    } else {
        resolve_revision(&git_dir, format, "HEAD")?
    };

    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let old_head = resolve_revision(&git_dir, format, "HEAD")?;
    let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
    sley_worktree::reset_index_and_worktree_to_commit(
        &worktree_root,
        &git_dir,
        format,
        &target_commit,
    )?;
    update_reset_head_ref(
        &git_dir,
        format,
        old_head,
        target_commit,
        "HEAD",
        commit_identity_from_env("COMMITTER")?,
    )?;

    clear_in_progress_merge_state(&git_dir);
    Ok(())
}

pub(crate) fn cmd_merge_continue() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.is_file() {
        eprintln!("fatal: There is no merge in progress (MERGE_HEAD missing).");
        return Err(GitError::Exit(128));
    }

    let format = repository_object_format(&git_dir)?;
    let message = read_merge_message_from_file(&git_dir)?;
    conclude_in_progress_merge(&git_dir, format, message, false)
}

pub(crate) fn conclude_in_progress_merge(
    git_dir: &Path,
    format: ObjectFormat,
    message: Vec<u8>,
    quiet: bool,
) -> Result<()> {
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.is_file() {
        eprintln!("fatal: There is no merge in progress (MERGE_HEAD missing).");
        return Err(GitError::Exit(128));
    }

    let index = read_worktree_index(git_dir, format)?;
    let unmerged_paths = index_unmerged_paths(&index);
    if !unmerged_paths.is_empty() {
        return report_unmerged_merge_continue(&unmerged_paths);
    }

    let ours_oid = resolve_revision(git_dir, format, "HEAD")?;
    let merge_head_contents = fs::read_to_string(&merge_head_path)?;
    let theirs_oid = ObjectId::from_hex(format, merge_head_contents.trim()).map_err(|_| {
        GitError::InvalidObject(format!(
            "invalid MERGE_HEAD value {}",
            merge_head_contents.trim()
        ))
    })?;
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let message = commit_cleanup_message(message, CommitCleanupMode::Whitespace);
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let mut writer = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let commit_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![ours_oid, theirs_oid],
            author,
            committer: committer.clone(),
            message: message.clone(),
        },
    )?;
    update_merge_head_ref(
        git_dir,
        format,
        ours_oid,
        commit_oid,
        "continue",
        merge_commit_reflog_message(&message),
        committer,
    )?;
    clear_in_progress_merge_state(git_dir);
    if !quiet {
        print_branch_commit_summary(git_dir, format, &commit_oid, &message)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebaseOntoOutcome {
    Rebasing,
    UpToDate,
}

fn rebase_onto_upstream(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    upstream: &str,
    quiet: bool,
) -> Result<RebaseOntoOutcome> {
    let store = FileRefStore::new(git_dir, format);
    let branch_name = store
        .current_branch()?
        .ok_or_else(|| GitError::Command("rebase requires a branch checkout".into()))?;
    let head_name = format!("refs/heads/{branch_name}");
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let head_oid = resolve_revision(git_dir, format, "HEAD")?;
    let head_commit = sley_rev::peel_to_commit(&db, format, &head_oid)?;
    let upstream_oid = if upstream == "FETCH_HEAD" {
        resolve_fetch_head_revision(git_dir, format)?
    } else {
        resolve_revision(git_dir, format, upstream)?
    };
    let upstream_commit = sley_rev::peel_to_commit(&db, format, &upstream_oid)?;

    let status = sley_worktree::short_status(worktree_root, git_dir, format)?;
    if !status.is_empty() {
        let has_staged = status.iter().any(|entry| entry.index != b' ');
        let has_unstaged = status.iter().any(|entry| entry.worktree != b' ');
        if has_unstaged && has_staged {
            eprintln!("error: cannot rebase: You have unstaged changes.");
            eprintln!("error: additionally, your index contains uncommitted changes.");
        } else if has_staged {
            eprintln!("error: cannot rebase: Your index contains uncommitted changes.");
        } else {
            eprintln!("error: cannot rebase: You have unstaged changes.");
        }
        eprintln!("error: Please commit or stash them.");
        return Err(GitError::Exit(1));
    }

    let merge_base = merge_bases(&db, format, &head_commit, &upstream_commit)?
        .into_iter()
        .next();
    let commits_to_replay =
        rebase_commits_to_replay(&db, format, &head_commit, merge_base.as_ref())?;
    if commits_to_replay.is_empty() {
        return Ok(RebaseOntoOutcome::UpToDate);
    }

    let committer = commit_identity_from_env("COMMITTER")?;
    sley_worktree::reset_index_and_worktree_to_commit(
        worktree_root,
        git_dir,
        format,
        &upstream_commit,
    )?;
    detach_head_at(
        git_dir,
        format,
        head_commit,
        upstream_commit,
        format!("checkout: moving to {upstream}").into_bytes(),
        committer.clone(),
    )?;

    let onto = upstream_commit;
    rebase_replay_commits(
        git_dir,
        worktree_root,
        format,
        &db,
        &head_name,
        &branch_name,
        &upstream_commit,
        &head_commit,
        &commits_to_replay,
        &commits_to_replay,
        onto,
        0,
        quiet,
        false,
    )?;
    Ok(RebaseOntoOutcome::Rebasing)
}

pub(crate) fn cmd_rebase(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut abort = false;
    let mut continue_rebase = false;
    let mut skip_rebase = false;
    let mut upstream = None;
    for arg in args {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--abort" => abort = true,
            "--no-abort" => abort = false,
            "--continue" => continue_rebase = true,
            "--no-continue" => continue_rebase = false,
            "--skip" => skip_rebase = true,
            "--no-skip" => skip_rebase = false,
            "--" => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "rebase currently supports --abort, --continue, --skip, --quiet, and an upstream argument; unsupported option {value}"
                )));
            }
            value => {
                if upstream.is_some() {
                    return Err(GitError::Command(
                        "rebase currently supports a single upstream argument".into(),
                    ));
                }
                upstream = Some(value.to_string());
            }
        }
    }
    if abort {
        if upstream.is_some() {
            print_rebase_usage();
            return Err(GitError::Exit(129));
        }
        return cmd_rebase_abort();
    }
    if continue_rebase {
        if upstream.is_some() || quiet {
            print_rebase_usage();
            return Err(GitError::Exit(129));
        }
        return cmd_rebase_continue();
    }
    if skip_rebase {
        if upstream.is_some() || quiet {
            print_rebase_usage();
            return Err(GitError::Exit(129));
        }
        return cmd_rebase_skip();
    }
    let Some(upstream) = upstream else {
        return Err(GitError::Command(
            "rebase currently requires an upstream argument".into(),
        ));
    };

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let branch_name = store
        .current_branch()?
        .ok_or_else(|| GitError::Command("rebase requires a branch checkout".into()))?;
    match rebase_onto_upstream(&git_dir, &worktree_root, format, &upstream, quiet)? {
        RebaseOntoOutcome::Rebasing => Ok(()),
        RebaseOntoOutcome::UpToDate => {
            println!("Current branch {branch_name} is up to date.");
            Ok(())
        }
    }
}

pub(crate) fn cmd_rebase_abort() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    if !rebase_in_progress(&git_dir) {
        eprintln!("fatal: no rebase in progress");
        return Err(GitError::Exit(128));
    }

    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let head_name = read_rebase_merge_file(&git_dir, "head-name")?;
    let orig_head_raw = read_rebase_merge_file(&git_dir, "orig-head")?;
    let orig_head = ObjectId::from_hex(format, orig_head_raw.trim()).map_err(|_| {
        GitError::InvalidObject(format!("invalid orig-head value {}", orig_head_raw.trim()))
    })?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let target_commit = sley_rev::peel_to_commit(&db, format, &orig_head)?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let old_head = resolve_revision(&git_dir, format, "HEAD")?;

    sley_worktree::reset_index_and_worktree_to_commit(
        &worktree_root,
        &git_dir,
        format,
        &target_commit,
    )?;
    finish_rebase_update_branch(
        &git_dir,
        format,
        &head_name,
        orig_head.clone(),
        target_commit,
        committer,
        old_head,
        "rebase (abort): returning to",
    )?;
    clear_in_progress_rebase_state(&git_dir);
    Ok(())
}

pub(crate) fn cmd_rebase_continue() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    if !rebase_in_progress(&git_dir) {
        eprintln!("fatal: no rebase in progress");
        return Err(GitError::Exit(128));
    }

    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let index = read_worktree_index(&git_dir, format)?;
    let unmerged_paths = index_unmerged_paths(&index);
    if !unmerged_paths.is_empty() {
        return report_unmerged_rebase_continue(&unmerged_paths);
    }

    let head_name = read_rebase_merge_file(&git_dir, "head-name")?;
    let branch_name = head_name
        .strip_prefix("refs/heads/")
        .unwrap_or(head_name.as_str())
        .to_string();
    let onto = ObjectId::from_hex(format, read_rebase_merge_file(&git_dir, "onto")?.trim())
        .map_err(|_| GitError::InvalidObject("invalid onto value during rebase".into()))?;
    let orig_head = ObjectId::from_hex(
        format,
        read_rebase_merge_file(&git_dir, "orig-head")?.trim(),
    )
    .map_err(|_| GitError::InvalidObject("invalid orig-head value during rebase".into()))?;
    let stopped_sha = ObjectId::from_hex(
        format,
        read_rebase_merge_file(&git_dir, "stopped-sha")?.trim(),
    )
    .map_err(|_| GitError::InvalidObject("invalid stopped-sha value during rebase".into()))?;
    let message = read_rebase_message_from_file(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let stopped_record = read_rev_list_commit_record(&db, format, stopped_sha.clone())?;

    let parent_oid = resolve_revision(&git_dir, format, "HEAD")?;
    let parent_tree = read_commit_tree(&db, format, &parent_oid)?;
    let tree = sley_worktree::write_tree_from_index(&git_dir, format)?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut writer = FileObjectDatabase::from_git_dir(&git_dir, format);
    let commit_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![parent_oid],
            author: stopped_record.commit.author.clone(),
            committer: committer.clone(),
            message: message.clone(),
        },
    )?;
    update_detached_head_at(
        &git_dir,
        format,
        parent_oid,
        commit_oid,
        format!(
            "rebase (continue): {}",
            commit_subject(&stopped_record.commit.message)
        )
        .into_bytes(),
        committer.clone(),
    )?;

    print_branch_commit_summary(&git_dir, format, &commit_oid, &message)?;
    print_commit_shortstat_between_trees(&db, format, &parent_tree, &tree)?;

    let all_commits = parse_rebase_pick_records(&git_dir, format, &db)?;
    let stopped_index = all_commits
        .iter()
        .position(|record| record.oid == stopped_sha)
        .unwrap_or(all_commits.len().saturating_sub(1));
    let remaining = all_commits
        .iter()
        .skip(stopped_index + 1)
        .cloned()
        .collect::<Vec<_>>();
    if remaining.is_empty() {
        finish_rebase_update_branch(
            &git_dir,
            format,
            &head_name,
            orig_head,
            commit_oid,
            committer,
            commit_oid,
            "rebase finished",
        )?;
        clear_rebase_merge_state(&git_dir);
        eprintln!("Successfully rebased and updated {head_name}.");
        return Ok(());
    }

    rebase_replay_commits(
        &git_dir,
        &worktree_root,
        format,
        &db,
        &head_name,
        &branch_name,
        &onto,
        &orig_head,
        &remaining,
        &all_commits,
        commit_oid,
        stopped_index + 1,
        true,
        true,
    )
}

pub(crate) fn cmd_rebase_skip() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    if !rebase_in_progress(&git_dir) {
        eprintln!("fatal: no rebase in progress");
        return Err(GitError::Exit(128));
    }

    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let head_name = read_rebase_merge_file(&git_dir, "head-name")?;
    let branch_name = head_name
        .strip_prefix("refs/heads/")
        .unwrap_or(head_name.as_str())
        .to_string();
    let onto = ObjectId::from_hex(format, read_rebase_merge_file(&git_dir, "onto")?.trim())
        .map_err(|_| GitError::InvalidObject("invalid onto value during rebase".into()))?;
    let orig_head = ObjectId::from_hex(
        format,
        read_rebase_merge_file(&git_dir, "orig-head")?.trim(),
    )
    .map_err(|_| GitError::InvalidObject("invalid orig-head value during rebase".into()))?;
    let stopped_sha = ObjectId::from_hex(
        format,
        read_rebase_merge_file(&git_dir, "stopped-sha")?.trim(),
    )
    .map_err(|_| GitError::InvalidObject("invalid stopped-sha value during rebase".into()))?;
    let current_head = resolve_revision(&git_dir, format, "HEAD")?;

    sley_worktree::reset_index_and_worktree_to_commit(
        &worktree_root,
        &git_dir,
        format,
        &current_head,
    )?;

    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let all_commits = parse_rebase_pick_records(&git_dir, format, &db)?;
    let stopped_index = all_commits
        .iter()
        .position(|record| record.oid == stopped_sha)
        .unwrap_or(all_commits.len().saturating_sub(1));
    let remaining = all_commits
        .iter()
        .skip(stopped_index + 1)
        .cloned()
        .collect::<Vec<_>>();
    if remaining.is_empty() {
        let committer = commit_identity_from_env("COMMITTER")?;
        finish_rebase_update_branch(
            &git_dir,
            format,
            &head_name,
            orig_head,
            current_head.clone(),
            committer,
            current_head,
            "rebase finished",
        )?;
        clear_rebase_merge_state(&git_dir);
        eprintln!("Successfully rebased and updated {head_name}.");
        return Ok(());
    }

    rebase_replay_commits(
        &git_dir,
        &worktree_root,
        format,
        &db,
        &head_name,
        &branch_name,
        &onto,
        &orig_head,
        &remaining,
        &all_commits,
        current_head,
        stopped_index + 1,
        false,
        true,
    )
}

fn rebase_merge_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("rebase-merge")
}

pub(crate) fn rebase_in_progress(git_dir: &Path) -> bool {
    rebase_merge_dir(git_dir).is_dir()
}

fn read_rebase_merge_file(git_dir: &Path, name: &str) -> Result<String> {
    let path = rebase_merge_dir(git_dir).join(name);
    if !path.is_file() {
        return Err(GitError::not_found(format!("rebase-merge/{name} missing")));
    }
    Ok(fs::read_to_string(path)?.trim_end_matches('\n').to_string())
}

fn clear_rebase_merge_state(git_dir: &Path) {
    let _ = fs::remove_dir_all(rebase_merge_dir(git_dir));
}

fn clear_in_progress_rebase_state(git_dir: &Path) {
    clear_rebase_merge_state(git_dir);
    let _ = fs::remove_file(git_dir.join("REBASE_HEAD"));
}

fn rebase_pick_line(record: &sley_rev::CommitRecord) -> String {
    format!(
        "pick {} # {}",
        record.oid.to_hex(),
        commit_subject(&record.commit.message)
    )
}

#[allow(clippy::too_many_arguments)]
fn write_rebase_conflict_state(
    git_dir: &Path,
    head_name: &str,
    onto: &ObjectId,
    orig_head: &ObjectId,
    record: &sley_rev::CommitRecord,
    commits_to_replay: &[sley_rev::CommitRecord],
    conflict_index: usize,
    conflicts: &[Vec<u8>],
) -> Result<()> {
    let dir = rebase_merge_dir(git_dir);
    fs::create_dir_all(&dir)?;

    let total = commits_to_replay.len();
    let msgnum = conflict_index + 1;

    fs::write(dir.join("head-name"), format!("{head_name}\n"))?;
    fs::write(dir.join("onto"), format!("{onto}\n"))?;
    fs::write(dir.join("orig-head"), format!("{orig_head}\n"))?;
    fs::write(dir.join("stopped-sha"), format!("{}\n", record.oid))?;
    fs::write(dir.join("msgnum"), format!("{msgnum}\n"))?;
    fs::write(dir.join("end"), format!("{total}\n"))?;

    let mut message = record.commit.message.clone();
    if !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(b"\n# Conflicts:\n");
    for conflict in conflicts {
        message.push(b'#');
        message.push(b'\t');
        message.extend_from_slice(conflict);
        message.push(b'\n');
    }
    fs::write(dir.join("message"), message)?;

    let done_lines = commits_to_replay[..=conflict_index]
        .iter()
        .map(rebase_pick_line)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.join("done"), format!("{done_lines}\n"))?;
    fs::write(dir.join("git-rebase-todo"), b"")?;

    let mut backup = String::new();
    for replay in commits_to_replay {
        backup.push_str(&rebase_pick_line(replay));
        backup.push('\n');
    }
    backup.push('\n');
    backup.push_str(&format!(
        "# Rebase {}..{} onto {} ({} command{})\n",
        &onto.to_hex()[..7.min(onto.to_hex().len())],
        &orig_head.to_hex()[..7.min(orig_head.to_hex().len())],
        &onto.to_hex()[..7.min(onto.to_hex().len())],
        total,
        if total == 1 { "" } else { "s" }
    ));
    fs::write(dir.join("git-rebase-todo.backup"), backup)?;
    Ok(())
}

fn read_rebase_message_from_file(git_dir: &Path) -> Result<Vec<u8>> {
    let raw = fs::read(rebase_merge_dir(git_dir).join("message"))?;
    let mut message = raw;
    if let Some(pos) = message
        .windows(12)
        .position(|window| window == b"\n# Conflicts:")
    {
        message.truncate(pos);
    }
    Ok(tag_stripspace_message(&message, true))
}

fn parse_rebase_pick_records(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<Vec<sley_rev::CommitRecord>> {
    let backup = fs::read_to_string(rebase_merge_dir(git_dir).join("git-rebase-todo.backup"))?;
    let mut records = Vec::new();
    for line in backup.lines() {
        let Some(rest) = line.strip_prefix("pick ") else {
            continue;
        };
        let sha = rest.split_whitespace().next().unwrap_or_default();
        let oid = ObjectId::from_hex(format, sha)
            .map_err(|_| GitError::InvalidObject(format!("invalid rebase pick oid {sha}")))?;
        records.push(read_rev_list_commit_record(db, format, oid)?);
    }
    Ok(records)
}

fn detach_head_at(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let reflog = ReflogEntry {
        old_oid,
        new_oid,
        committer,
        message: reflog_message,
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog: Some(reflog),
    });
    tx.commit()
}

fn update_detached_head_at(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    detach_head_at(git_dir, format, old_oid, new_oid, reflog_message, committer)
}

#[allow(clippy::too_many_arguments)]
fn finish_rebase_update_branch(
    git_dir: &Path,
    format: ObjectFormat,
    head_name: &str,
    old_branch_oid: ObjectId,
    new_oid: ObjectId,
    committer: Vec<u8>,
    old_head_oid: ObjectId,
    reflog_prefix: &str,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let branch_reflog = ReflogEntry {
        old_oid: old_branch_oid,
        new_oid,
        committer: committer.clone(),
        message: format!("{reflog_prefix}: {head_name}").into_bytes(),
    };
    let head_reflog = ReflogEntry {
        old_oid: old_head_oid,
        new_oid,
        committer,
        message: format!("{reflog_prefix}: {head_name}").into_bytes(),
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: head_name.into(),
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog: Some(branch_reflog),
    });
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(head_name.into()),
        reflog: Some(head_reflog),
    });
    tx.commit()
}

fn print_commit_shortstat_between_trees(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
) -> Result<()> {
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    if entries.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    write_diff_shortstat(&mut stdout, &entries, db, None, false)?;
    Ok(())
}

fn print_rebase_conflict_hints() {
    eprintln!("hint: Resolve all conflicts manually, mark them as resolved with");
    eprintln!("hint: \"git add/rm <conflicted_files>\", then run \"git rebase --continue\".");
    eprintln!("hint: You can instead skip this commit: run \"git rebase --skip\".");
    eprintln!(
        "hint: To abort and get back to the state before \"git rebase\", run \"git rebase --abort\"."
    );
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

fn report_unmerged_rebase_continue(unmerged_paths: &[Vec<u8>]) -> Result<()> {
    if let Some(path) = unmerged_paths.first() {
        eprintln!("{}: needs merge", status_quote_path(path, false));
    }
    eprintln!("You must edit all merge conflicts and then");
    eprintln!("mark them as resolved using git add");
    Err(GitError::Exit(1))
}

pub(crate) fn conclude_rebase_step_via_commit(
    git_dir: &Path,
    format: ObjectFormat,
    author: Vec<u8>,
    committer: Vec<u8>,
    message: Vec<u8>,
    quiet: bool,
) -> Result<()> {
    let index = read_worktree_index(git_dir, format)?;
    let unmerged_paths = index_unmerged_paths(&index);
    if !unmerged_paths.is_empty() {
        return report_unmerged_merge_continue(&unmerged_paths);
    }

    let parent_oid = resolve_revision(git_dir, format, "HEAD")?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let parent_tree = read_commit_tree(&db, format, &parent_oid)?;
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    let mut writer = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![parent_oid],
            author,
            committer: committer.clone(),
            message: message.clone(),
        },
    )?;
    update_detached_head_at(
        git_dir,
        format,
        parent_oid,
        commit_oid,
        commit_reflog_message(&message, false),
        committer,
    )?;

    if !quiet {
        print_branch_commit_summary(git_dir, format, &commit_oid, &message)?;
        print_commit_shortstat_between_trees(&db, format, &parent_tree, &tree)?;
    }
    Ok(())
}

fn print_rebase_usage() {
    eprintln!(
        "usage: git rebase [-i] [options] [--exec <cmd>] [--onto <newbase> | --keep-base] [<upstream> [<branch>]]"
    );
    eprintln!(
        "   or: git rebase [-i] [options] [--exec <cmd>] [--onto <newbase>] --root [<branch>]"
    );
    eprintln!("   or: git rebase --continue | --abort | --skip | --edit-todo");
    eprintln!();
    eprintln!("    --[no-]onto <revision>");
    eprintln!("                          rebase onto given branch instead of upstream");
    eprintln!(
        "    --[no-]keep-base      use the merge-base of upstream and branch as the current base"
    );
    eprintln!("    --no-verify           allow pre-rebase hook to run");
    eprintln!("    --verify              opposite of --no-verify");
    eprintln!("    -q, --[no-]quiet      be quiet. implies --no-stat");
    eprintln!("    -v, --[no-]verbose    display a diffstat of what changed upstream");
    eprintln!("    -n, --no-stat         do not show diffstat of what changed upstream");
    eprintln!("    --stat                opposite of --no-stat");
    eprintln!("    --[no-]trailer <trailer>");
    eprintln!("                          add custom trailer(s)");
    eprintln!("    --[no-]signoff        add a Signed-off-by trailer to each commit");
    eprintln!("    --[no-]committer-date-is-author-date");
    eprintln!("                          make committer date match author date");
    eprintln!("    --[no-]reset-author-date");
    eprintln!("                          ignore author date and use current date");
    eprintln!("    -C <n>                passed to 'git apply'");
    eprintln!("    --[no-]ignore-whitespace");
    eprintln!("                          ignore changes in whitespace");
    eprintln!("    --[no-]whitespace <action>");
    eprintln!("                          passed to 'git apply'");
    eprintln!("    -f, --[no-]force-rebase");
    eprintln!("                          cherry-pick all commits, even if unchanged");
    eprintln!("    --no-ff               cherry-pick all commits, even if unchanged");
    eprintln!("    --ff                  opposite of --no-ff");
    eprintln!("    --continue            continue");
    eprintln!("    --skip                skip current patch and continue");
    eprintln!("    --abort               abort and check out the original branch");
    eprintln!("    --quit                abort but keep HEAD where it is");
    eprintln!("    --edit-todo           edit the todo list during an interactive rebase");
    eprintln!("    --show-current-patch  show the patch file being applied or merged");
    eprintln!("    --apply               use apply strategies to rebase");
    eprintln!("    -m, --merge           use merging strategies to rebase");
    eprintln!("    -i, --interactive     let the user edit the list of commits to rebase");
    eprintln!("    --[no-]rerere-autoupdate");
    eprintln!(
        "                          update the index with reused conflict resolution if possible"
    );
    eprintln!("    --empty (drop|keep|stop)");
    eprintln!("                          how to handle commits that become empty");
    eprintln!("    --[no-]autosquash     move commits that begin with squash!/fixup! under -i");
    eprintln!(
        "    --[no-]update-refs    update branches that point to commits that are being rebased"
    );
    eprintln!("    -S, --[no-]gpg-sign[=<key-id>]");
    eprintln!("                          GPG-sign commits");
    eprintln!("    --[no-]autostash      automatically stash/stash pop before and after");
    eprintln!("    -x, --[no-]exec <exec>");
    eprintln!("                          add exec lines after each commit of the editable list");
    eprintln!("    -r, --[no-]rebase-merges[=<mode>]");
    eprintln!("                          try to rebase merges instead of skipping them");
    eprintln!("    --[no-]fork-point     use 'merge-base --fork-point' to refine upstream");
    eprintln!("    -s, --[no-]strategy <strategy>");
    eprintln!("                          use the given merge strategy");
    eprintln!("    -X, --[no-]strategy-option <option>");
    eprintln!("                          pass the argument through to the merge strategy");
    eprintln!("    --[no-]root           rebase all reachable commits up to the root(s)");
    eprintln!("    --[no-]reschedule-failed-exec");
    eprintln!("                          automatically re-schedule any `exec` that fails");
    eprintln!("    --[no-]reapply-cherry-picks");
    eprintln!("                          apply all changes, even those already present upstream");
    eprintln!();
}

#[allow(clippy::too_many_arguments)]
fn rebase_replay_commits(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    head_name: &str,
    branch_name: &str,
    onto: &ObjectId,
    orig_head: &ObjectId,
    commits_to_replay: &[sley_rev::CommitRecord],
    all_commits: &[sley_rev::CommitRecord],
    mut current_head: ObjectId,
    start_offset: usize,
    quiet: bool,
    finishing_after_continue: bool,
) -> Result<()> {
    let total = all_commits.len();
    let committer = commit_identity_from_env("COMMITTER")?;
    let progress_to_stdout = io::stdout().is_terminal();
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    for (index, record) in commits_to_replay.iter().enumerate() {
        let msgnum = start_offset + index + 1;
        if !quiet {
            let progress = format!("Rebasing ({msgnum}/{total})\r");
            if progress_to_stdout {
                print!("{progress}");
                io::stdout().flush()?;
            } else {
                eprint!("{progress}");
                io::stderr().flush()?;
            }
        }
        let parent_oid = record.parents.first().ok_or_else(|| {
            GitError::InvalidObject(format!(
                "cannot replay root commit {} during rebase",
                record.oid
            ))
        })?;
        let parent_tree = read_commit_tree(db, format, parent_oid)?;
        let ours_tree = read_commit_tree(db, format, &current_head)?;
        let theirs_tree = record.commit.tree;
        let base_map = stash_tree_entry_map(db, format, &parent_tree)?;
        let ours_map = stash_tree_entry_map(db, format, &ours_tree)?;
        let theirs_map = stash_tree_entry_map(db, format, &theirs_tree)?;
        let mut write_db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
        let (results, conflicts) = three_way_merge_trees(
            &mut write_db,
            &base_map,
            &ours_map,
            &theirs_map,
            "HEAD",
            branch_name,
        )?;
        let auto_merged_paths = results
            .iter()
            .filter_map(|(path, result)| {
                if let MergePathResult::Resolved(Some((mode, oid))) = result
                    && ours_map.get(path) != Some(&(*mode, *oid))
                {
                    return Some(path.clone());
                }
                None
            })
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            let mut entries = Vec::new();
            for (path, result) in &results {
                match result {
                    MergePathResult::Resolved(Some((mode, oid))) => {
                        entries.push(merge_index_entry(path, *mode, *oid, 0));
                    }
                    MergePathResult::Resolved(None) => {}
                    MergePathResult::Conflict {
                        base, ours, theirs, ..
                    } => {
                        if let Some((mode, oid)) = base {
                            entries.push(merge_index_entry(path, *mode, *oid, 1));
                        }
                        if let Some((mode, oid)) = ours {
                            entries.push(merge_index_entry(path, *mode, *oid, 2));
                        }
                        if let Some((mode, oid)) = theirs {
                            entries.push(merge_index_entry(path, *mode, *oid, 3));
                        }
                    }
                }
            }
            entries.sort_by(|left, right| {
                left.path
                    .cmp(&right.path)
                    .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
            });
            fs::write(
                sley_worktree::repository_index_path(git_dir),
                Index {
                    version: 2,
                    entries,
                    extensions: Vec::new(),
                    checksum: None,
                }
                .write(format)?,
            )?;
            for (path, result) in &results {
                match result {
                    MergePathResult::Resolved(Some((mode, oid))) => {
                        if ours_map.get(path) != Some(&(*mode, *oid)) {
                            let content = merge_read_blob(db, oid)?;
                            merge_write_worktree_file(worktree_root, path, &content, *mode)?;
                        }
                    }
                    MergePathResult::Resolved(None) => {
                        merge_remove_worktree_file(worktree_root, path)?
                    }
                    MergePathResult::Conflict { worktree, .. } => match worktree {
                        Some((mode, content)) => {
                            merge_write_worktree_file(worktree_root, path, content, *mode)?
                        }
                        None => merge_remove_worktree_file(worktree_root, path)?,
                    },
                }
            }
            let merged_tree = sley_worktree::write_tree_from_index(git_dir, format)?;
            write_rebase_conflict_state(
                git_dir,
                head_name,
                onto,
                orig_head,
                record,
                all_commits,
                start_offset + index,
                &conflicts,
            )?;
            fs::write(git_dir.join("REBASE_HEAD"), format!("{}\n", record.oid))?;
            let conflict_set = conflicts.iter().cloned().collect::<BTreeSet<_>>();
            for path in &auto_merged_paths {
                if !conflict_set.contains(path) {
                    println!("Auto-merging {}", String::from_utf8_lossy(path));
                }
            }
            for path in &conflicts {
                let path = String::from_utf8_lossy(path);
                println!("Auto-merging {path}");
                println!("CONFLICT (content): Merge conflict in {path}");
            }
            let short_oid = &record.oid.to_hex()[..7.min(record.oid.to_hex().len())];
            let subject = commit_subject(&record.commit.message);
            eprintln!("error: could not apply {short_oid}... {subject}");
            print_rebase_conflict_hints();
            eprintln!("Could not apply {short_oid}... # {subject}");
            let _ = merged_tree;
            return Err(GitError::Exit(1));
        }
        for path in &auto_merged_paths {
            println!("Auto-merging {}", String::from_utf8_lossy(path));
        }
        let mut entries = Vec::new();
        for (path, result) in &results {
            if let MergePathResult::Resolved(Some((mode, oid))) = result {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        fs::write(
            sley_worktree::repository_index_path(git_dir),
            Index {
                version: 2,
                entries,
                extensions: Vec::new(),
                checksum: None,
            }
            .write(format)?,
        )?;
        let merged_tree = sley_worktree::write_tree_from_index(git_dir, format)?;
        sley_worktree::checkout_tree_to_index_and_worktree(
            worktree_root,
            git_dir,
            format,
            &merged_tree,
        )?;
        let mut writer = FileObjectDatabase::from_git_dir(&common_git_dir, format);
        let commit_oid = sley_sequencer::create_commit(
            &mut writer,
            sley_sequencer::CommitCreate {
                tree: merged_tree,
                parents: vec![current_head.clone()],
                author: record.commit.author.clone(),
                committer: committer.clone(),
                message: record.commit.message.clone(),
            },
        )?;
        update_detached_head_at(
            git_dir,
            format,
            current_head,
            commit_oid,
            format!("rebase (pick): {}", commit_subject(&record.commit.message)).into_bytes(),
            committer.clone(),
        )?;
        current_head = commit_oid;
    }
    finish_rebase_update_branch(
        git_dir,
        format,
        head_name,
        orig_head.clone(),
        current_head.clone(),
        committer,
        current_head.clone(),
        "rebase finished",
    )?;
    clear_rebase_merge_state(git_dir);
    if !quiet {
        let message = format!("Successfully rebased and updated {head_name}.\n");
        if finishing_after_continue || !progress_to_stdout {
            eprint!("{message}");
        } else {
            print!("{message}");
        }
    }
    Ok(())
}

fn rebase_commits_to_replay(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head: &ObjectId,
    merge_base: Option<&ObjectId>,
) -> Result<Vec<sley_rev::CommitRecord>> {
    let mut commits = Vec::new();
    let mut current = head.clone();
    loop {
        if merge_base.is_some_and(|base| current == *base) {
            break;
        }
        let record = read_rev_list_commit_record(db, format, current)?;
        let parent = record.parents.first().cloned();
        commits.push(record);
        current = match parent {
            Some(parent) => parent,
            None => break,
        };
    }
    commits.reverse();
    Ok(commits)
}
fn clear_in_progress_merge_state(git_dir: &Path) {
    let _ = fs::remove_file(git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("MERGE_MODE"));
}

fn read_worktree_index(git_dir: &Path, format: ObjectFormat) -> Result<Index> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    Index::parse(&fs::read(index_path)?, format)
}

fn index_unmerged_paths(index: &Index) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for entry in &index.entries {
        if index_entry_stage(entry) > 0 {
            paths.insert(entry.path.clone());
        }
    }
    paths.into_iter().map(|path| path.into_bytes()).collect()
}

fn report_unmerged_merge_continue(unmerged_paths: &[Vec<u8>]) -> Result<()> {
    eprintln!("error: Committing is not possible because you have unmerged files.");
    eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
    eprintln!("hint: as appropriate to mark resolution and make a commit.");
    eprintln!("fatal: Exiting because of an unresolved conflict.");
    let mut stdout = io::stdout().lock();
    for path in unmerged_paths {
        write!(stdout, "U\t")?;
        stdout.write_all(status_quote_path(path, false).as_bytes())?;
        stdout.write_all(b"\n")?;
    }
    Err(GitError::Exit(128))
}

pub(crate) fn read_merge_message_from_file(git_dir: &Path) -> Result<Vec<u8>> {
    let merge_msg_path = git_dir.join("MERGE_MSG");
    let raw = if merge_msg_path.is_file() {
        fs::read(merge_msg_path)?
    } else {
        b"Merge commit\n".to_vec()
    };
    Ok(tag_stripspace_message(&raw, true))
}

fn merge_commit_reflog_message(message: &[u8]) -> Vec<u8> {
    format!("commit (merge): {}", commit_subject(message)).into_bytes()
}

fn print_branch_commit_summary(
    git_dir: &Path,
    format: ObjectFormat,
    commit_oid: &ObjectId,
    message: &[u8],
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let ref_name = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => name
            .strip_prefix("refs/heads/")
            .unwrap_or(name.as_str())
            .to_string(),
        Some(RefTarget::Direct(_)) => "detached HEAD".into(),
        _ => "HEAD".into(),
    };
    println!(
        "[{ref_name} {}] {}",
        format_log_abbrev_oid(commit_oid),
        commit_subject(message)
    );
    Ok(())
}

fn read_commit_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {commit_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

fn update_merge_head_ref(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    _branch: &str,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let reflog = ReflogEntry {
        old_oid,
        new_oid,
        committer,
        message: reflog_message,
    };
    let mut tx = store.transaction();
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => {
            tx.update(RefUpdate {
                name: name.clone(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog.clone()),
            });
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(name),
                reflog: Some(reflog),
            });
        }
        _ => {
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog),
            });
        }
    }
    tx.commit()
}
fn resolve_pull_remote_and_refspecs(
    config: &GitConfig,
    store: &FileRefStore,
    remote: Option<String>,
    branch: Option<String>,
) -> Result<(String, Vec<String>, Option<String>)> {
    match (remote, branch) {
        (Some(remote), Some(branch)) => {
            let refspec = format!("refs/heads/{branch}");
            Ok((remote, vec![refspec], Some(format!("refs/heads/{branch}"))))
        }
        (Some(remote), None) => {
            let merge_src = store.current_branch().ok().flatten().and_then(|current| {
                config
                    .get("branch", Some(&current), "merge")
                    .map(str::to_string)
            });
            Ok((remote, Vec::new(), merge_src))
        }
        (None, None) => {
            let Some(current) = store.current_branch()? else {
                eprintln!("There is no tracking information for the current branch.");
                eprintln!("Please specify which branch you want to merge with.");
                eprintln!("See git-pull(1) for details.");
                eprintln!();
                eprintln!("    git pull <remote> <branch>");
                eprintln!();
                eprintln!(
                    "If you wish to set tracking information for this branch you can do so with:"
                );
                eprintln!();
                eprintln!("    git branch --set-upstream-to=<remote>/<branch> HEAD");
                eprintln!();
                return Err(GitError::Exit(1));
            };
            let Some(remote) = config.get("branch", Some(&current), "remote") else {
                eprintln!("There is no tracking information for the current branch.");
                eprintln!("Please specify which branch you want to merge with.");
                eprintln!("See git-pull(1) for details.");
                eprintln!();
                eprintln!("    git pull <remote> <branch>");
                eprintln!();
                eprintln!(
                    "If you wish to set tracking information for this branch you can do so with:"
                );
                eprintln!();
                eprintln!("    git branch --set-upstream-to=<remote>/<branch> {current}");
                eprintln!();
                return Err(GitError::Exit(1));
            };
            let Some(merge) = config.get("branch", Some(&current), "merge") else {
                eprintln!("There is no tracking information for the current branch.");
                eprintln!("Please specify which branch you want to merge with.");
                eprintln!("See git-pull(1) for details.");
                eprintln!();
                eprintln!("    git pull <remote> <branch>");
                eprintln!();
                eprintln!(
                    "If you wish to set tracking information for this branch you can do so with:"
                );
                eprintln!();
                eprintln!("    git branch --set-upstream-to=<remote>/<branch> {current}");
                eprintln!();
                return Err(GitError::Exit(1));
            };
            Ok((remote.to_string(), Vec::new(), Some(merge.to_string())))
        }
        (None, Some(_)) => Err(GitError::Command(
            "pull currently requires a remote when a branch is specified".into(),
        )),
    }
}

fn fetch_head_merge_record(git_dir: &Path, format: ObjectFormat) -> Result<FetchHeadRecord> {
    let path = git_dir.join("FETCH_HEAD");
    let mut input =
        fs::File::open(path).map_err(|_| GitError::reference_not_found("FETCH_HEAD"))?;
    let records = read_fetch_head(format, &mut input)?;
    records
        .into_iter()
        .find(|record| !record.not_for_merge)
        .ok_or_else(|| GitError::reference_not_found("FETCH_HEAD"))
}

fn resolve_fetch_head_revision(git_dir: &Path, format: ObjectFormat) -> Result<ObjectId> {
    Ok(fetch_head_merge_record(git_dir, format)?.oid)
}

fn ensure_pull_can_merge(config: &GitConfig) -> Result<()> {
    if config.get("pull", None, "rebase").is_none() {
        eprintln!("hint: You have divergent branches and need to specify how to reconcile them.");
        eprintln!("hint: You can do so by running one of the following commands sometime before");
        eprintln!("hint: your next pull:");
        eprintln!("hint:");
        eprintln!("hint:   git config pull.rebase false  # merge");
        eprintln!("hint:   git config pull.rebase true   # rebase");
        eprintln!("hint:   git config pull.ff only       # fast-forward only");
        eprintln!("hint:");
        eprintln!(
            "hint: You can replace \"git config\" with \"git config --global\" to set a default"
        );
        eprintln!(
            "hint: preference for all repositories. You can also pass --rebase, --no-rebase,"
        );
        eprintln!("hint: or --ff-only on the command line to override the configured default per");
        eprintln!("hint: invocation.");
        eprintln!("fatal: Need to specify how to reconcile divergent branches.");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn print_fetch_status(
    source: &str,
    updates: &[FetchRefUpdate],
    old_oids: &HashMap<String, ObjectId>,
) {
    let mut displayed = false;
    for update in updates {
        let src_short = update
            .src
            .strip_prefix("refs/heads/")
            .unwrap_or(update.src.as_str());
        let Some(dst) = update.dst.as_ref() else {
            if !displayed {
                eprintln!("From {source}");
                displayed = true;
            }
            eprintln!(" * branch            {src_short:11}-> FETCH_HEAD");
            continue;
        };
        if old_oids.get(dst) == Some(&update.oid) {
            continue;
        }
        if !displayed {
            eprintln!("From {source}");
            displayed = true;
        }
        let dst_short = dst.strip_prefix("refs/remotes/").unwrap_or(dst.as_str());
        let old_short = old_oids
            .get(dst)
            .map(format_log_abbrev_oid)
            .unwrap_or_else(|| "0000000".to_string());
        eprintln!(
            "   {}..{}  {:11} -> {}",
            old_short,
            format_log_abbrev_oid(&update.oid),
            src_short,
            dst_short
        );
    }
}

fn pull_fetch(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<FetchOutcome> {
    if let Ok(input) = fs::read(remote)
        && let Ok(bundle) = Bundle::parse(&input, format)
    {
        fetch_bundle(git_dir, format, remote, refspecs, &bundle, options)?;
        return Ok(FetchOutcome::default());
    }
    if fetch_source_is_ssh(remote)? {
        fetch_ssh_repository(git_dir, format, remote, refspecs, options)?;
        Ok(FetchOutcome::default())
    } else {
        let config = read_repo_config(git_dir)?;
        let remote_git_dir = ls_remote_git_dir(remote)?;
        let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
        let fetch_source = sley_remote::FetchSource::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        };
        let store = FileRefStore::new(git_dir, format);
        let mut old_oids = HashMap::new();
        if options.merge_src.is_some() {
            for update_dst in store.list_refs()? {
                if let Some((oid, _)) = resolve_for_each_ref_target(&store, &update_dst)? {
                    old_oids.insert(update_dst.name, oid);
                }
            }
        }
        let quiet = options.quiet;
        let outcome = run_fetch_with_outcome(
            git_dir,
            format,
            &config,
            remote,
            &fetch_source,
            refspecs,
            options,
        )?;
        if !quiet {
            print_fetch_status(remote, &outcome.ref_updates, &old_oids);
        }
        Ok(outcome)
    }
}

fn run_fetch_with_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    source: &str,
    fetch_source: &sley_remote::FetchSource,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<FetchOutcome> {
    let mut credentials = sley_remote::CredentialHelperProvider::new(Some(config));
    let mut progress = StdoutProgress;
    sley_remote::fetch(
        sley_remote::FetchRequest {
            git_dir,
            format,
            config,
            remote_name: source,
            source: fetch_source,
            refspecs,
            options: &options,
        },
        sley_remote::FetchServices {
            credentials: &mut credentials,
            progress: &mut progress,
        },
    )
}

pub(crate) fn cmd_pull(args: &[String]) -> Result<()> {
    let mut no_ff = false;
    let mut ff_only = false;
    let mut quiet = false;
    let mut rebase_flag = None::<bool>;
    let mut remote = None::<String>;
    let mut branch = None::<String>;
    for arg in args {
        match arg.as_str() {
            "--no-ff" => no_ff = true,
            "--ff-only" => ff_only = true,
            "--rebase" => rebase_flag = Some(true),
            "--no-rebase" => rebase_flag = Some(false),
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "pull currently supports --ff-only, --no-ff, --rebase, --no-rebase, --quiet, and remote/branch arguments; unsupported option {value}"
                )));
            }
            value => {
                if remote.is_none() {
                    remote = Some(value.to_string());
                } else if branch.is_none() {
                    branch = Some(value.to_string());
                } else {
                    return Err(GitError::Command(
                        "pull accepts at most one remote and one branch".into(),
                    ));
                }
            }
        }
    }
    if ff_only && no_ff {
        return Err(GitError::Command(
            "pull cannot combine --ff-only and --no-ff".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let (remote, refspecs, merge_src) =
        resolve_pull_remote_and_refspecs(&config, &store, remote, branch)?;
    let config_ff_only = config
        .get("pull", None, "ff")
        .is_some_and(|value| value == "only");
    let effective_ff_only = ff_only || config_ff_only;
    let effective_rebase = match rebase_flag {
        Some(value) => value,
        None => config.get("pull", None, "rebase") == Some("true"),
    };
    let fetch_options = FetchOptions {
        quiet,
        auto_follow_tags: true,
        fetch_all_tags: false,
        prune: false,
        dry_run: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: false,
        prune_option_explicit: false,
        depth: None,
        merge_src,
    };
    pull_fetch(&git_dir, format, &remote, &refspecs, fetch_options)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let ours_oid = resolve_revision(&git_dir, format, "HEAD")?;
    let theirs_oid = resolve_fetch_head_revision(&git_dir, format)?;
    let ours_commit = sley_rev::peel_to_commit(&db, format, &ours_oid)?;
    let theirs_commit = sley_rev::peel_to_commit(&db, format, &theirs_oid)?;
    if ours_commit == theirs_commit {
        if !quiet {
            println!("Already up to date.");
        }
        return Ok(());
    }
    let ours_depths = ancestor_depths(&db, format, &ours_commit)?;
    if ours_depths.contains_key(&theirs_commit) {
        if !quiet {
            println!("Already up to date.");
        }
        return Ok(());
    }
    let theirs_depths = ancestor_depths(&db, format, &theirs_commit)?;
    let fast_forward = theirs_depths.contains_key(&ours_commit);
    if fast_forward {
        let mut merge_args = Vec::new();
        if no_ff {
            merge_args.push("--no-ff".to_string());
        }
        if effective_ff_only {
            merge_args.push("--ff-only".to_string());
        }
        if quiet {
            merge_args.push("--quiet".to_string());
        }
        merge_args.push("FETCH_HEAD".to_string());
        return cmd_merge(&merge_args);
    }
    if effective_ff_only {
        eprintln!("fatal: Not possible to fast-forward, aborting.");
        return Err(GitError::Exit(128));
    }
    if effective_rebase {
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        match rebase_onto_upstream(&git_dir, &worktree_root, format, "FETCH_HEAD", quiet)? {
            RebaseOntoOutcome::Rebasing => return Ok(()),
            RebaseOntoOutcome::UpToDate => {
                if !quiet {
                    println!("Already up to date.");
                }
                return Ok(());
            }
        }
    }
    ensure_pull_can_merge(&config)?;
    let mut merge_args = Vec::new();
    if no_ff {
        merge_args.push("--no-ff".to_string());
    }
    if effective_ff_only {
        merge_args.push("--ff-only".to_string());
    }
    if quiet {
        merge_args.push("--quiet".to_string());
    }
    merge_args.push("FETCH_HEAD".to_string());
    cmd_merge(&merge_args)
}

pub(crate) fn commit_tree_oid(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {commit_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

// ===== cherry-pick / revert (single-commit 3-way replay) =====

pub(crate) fn head_commit_oid(refs: &FileRefStore) -> Result<Option<ObjectId>> {
    match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            _ => Ok(None),
        },
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        None => Ok(None),
    }
}

struct ReplayPlan {
    base: MergeTreeMap,
    theirs: MergeTreeMap,
    theirs_label: String,
    new_parents: Vec<ObjectId>,
    author: Vec<u8>,
    committer: Vec<u8>,
    message: Vec<u8>,
    state_file: &'static str,
    state_oid: ObjectId,
    reflog_message: Vec<u8>,
    conflict_error: String,
}

/// Replay a single commit's change onto HEAD via 3-way merge (the shared core of
/// cherry-pick and revert). On a clean result, creates the commit and advances
/// HEAD; on conflict, writes a staged index + worktree + `<state_file>` and exits 1.
fn finalize_replay(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    refs: &FileRefStore,
    head_oid: &ObjectId,
    plan: ReplayPlan,
) -> Result<()> {
    let read_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let head_tree = commit_tree_oid(&read_db, format, head_oid)?;
    let ours_map = stash_tree_entry_map(&read_db, format, &head_tree)?;
    let mut write_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let (results, conflicts) = three_way_merge_trees(
        &mut write_db,
        &plan.base,
        &ours_map,
        &plan.theirs,
        "HEAD",
        &plan.theirs_label,
    )?;

    if conflicts.is_empty() {
        let mut entries = Vec::new();
        for (path, result) in &results {
            if let MergePathResult::Resolved(Some((mode, oid))) = result {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let index = Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            sley_worktree::repository_index_path(git_dir),
            index.write(format)?,
        )?;
        let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
        let mut commit_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
        let new_oid = sley_sequencer::create_commit(
            &mut commit_db,
            sley_sequencer::CommitCreate {
                tree,
                parents: plan.new_parents,
                author: plan.author,
                committer: plan.committer.clone(),
                message: plan.message,
            },
        )?;
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: Some(RefTarget::Direct(*head_oid)),
            new: RefTarget::Direct(new_oid),
            reflog: Some(ReflogEntry {
                old_oid: *head_oid,
                new_oid,
                committer: plan.committer,
                message: plan.reflog_message,
            }),
        });
        tx.commit()?;
        sley_worktree::reset_index_and_worktree_to_commit(
            worktree_root,
            git_dir,
            format,
            &new_oid,
        )?;
        return Ok(());
    }

    let mut entries = Vec::new();
    for (path, result) in &results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict {
                base, ours, theirs, ..
            } => {
                if let Some((mode, oid)) = base {
                    entries.push(merge_index_entry(path, *mode, *oid, 1));
                }
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 2));
                }
                if let Some((mode, oid)) = theirs {
                    entries.push(merge_index_entry(path, *mode, *oid, 3));
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags >> 12).cmp(&(right.flags >> 12)))
    });
    let index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        index.write(format)?,
    )?;
    for (path, result) in &results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = merge_read_blob(&read_db, oid)?;
                    merge_write_worktree_file(worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => merge_remove_worktree_file(worktree_root, path)?,
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    merge_write_worktree_file(worktree_root, path, content, *mode)?
                }
                None => merge_remove_worktree_file(worktree_root, path)?,
            },
        }
    }
    fs::write(
        git_dir.join(plan.state_file),
        format!("{}\n", plan.state_oid),
    )?;
    let mut merge_msg = plan.message.clone();
    merge_msg.extend_from_slice(b"\nConflicts:\n");
    for path in &conflicts {
        merge_msg.extend_from_slice(format!("\t{}\n", String::from_utf8_lossy(path)).as_bytes());
    }
    fs::write(git_dir.join("MERGE_MSG"), merge_msg)?;
    fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;
    for path in &conflicts {
        println!(
            "CONFLICT (content): Merge conflict in {}",
            String::from_utf8_lossy(path)
        );
    }
    eprintln!("{}", plan.conflict_error);
    Err(GitError::Exit(1))
}

fn sequencer_abort(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_file: &str,
) -> Result<()> {
    if !git_dir.join(state_file).exists() {
        return Err(GitError::Command(format!(
            "no cherry-pick or revert in progress ({state_file} missing)"
        )));
    }
    let head_oid = resolve_revision(git_dir, format, "HEAD")?;
    sley_worktree::reset_index_and_worktree_to_commit(worktree_root, git_dir, format, &head_oid)?;
    for name in [state_file, "MERGE_MSG", "ORIG_HEAD"] {
        let path = git_dir.join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

pub(crate) fn cmd_cherry_pick(args: &[String]) -> Result<()> {
    let mut abort = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--abort" => abort = true,
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported cherry-pick option {value}"
                )));
            }
            value => positional.push(value.to_string()),
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let refs = FileRefStore::new(&git_dir, format);
    if abort {
        return sequencer_abort(&git_dir, &worktree_root, format, "CHERRY_PICK_HEAD");
    }
    let target = match positional.as_slice() {
        [target] => target.clone(),
        [] => return Err(GitError::Command("cherry-pick requires a commit".into())),
        _ => {
            return Err(GitError::Unsupported(
                "cherry-pick of multiple commits is not supported yet".into(),
            ));
        }
    };
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let pick = read_reused_commit(&git_dir, format, &target)?;
    let pick_oid =
        sley_rev::peel_to_commit(&db, format, &resolve_revision(&git_dir, format, &target)?)?;
    let head_oid = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("cherry-pick onto unborn HEAD is not supported".into()))?;
    let theirs_map = stash_tree_entry_map(&db, format, &pick.tree)?;
    let base_map = match pick.parents.first() {
        Some(parent) => {
            let parent_tree = commit_tree_oid(&db, format, parent)?;
            stash_tree_entry_map(&db, format, &parent_tree)?
        }
        None => MergeTreeMap::new(),
    };
    let committer = commit_identity_from_env("COMMITTER")?;
    let subject = commit_subject(&pick.message);
    finalize_replay(
        &git_dir,
        &common_git_dir,
        &worktree_root,
        format,
        &refs,
        &head_oid,
        ReplayPlan {
            base: base_map,
            theirs: theirs_map,
            theirs_label: format!("{} ({subject})", format_log_abbrev_oid(&pick_oid)),
            new_parents: vec![head_oid],
            author: pick.author.clone(),
            committer,
            message: pick.message.clone(),
            state_file: "CHERRY_PICK_HEAD",
            state_oid: pick_oid,
            reflog_message: format!("cherry-pick: {subject}").into_bytes(),
            conflict_error: format!(
                "error: could not apply {}... {subject}",
                &pick_oid.to_hex()[..7.min(pick_oid.to_hex().len())]
            ),
        },
    )
}

pub(crate) fn cmd_revert(args: &[String]) -> Result<()> {
    let mut abort = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--abort" => abort = true,
            "--no-edit" => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported revert option {value}"
                )));
            }
            value => positional.push(value.to_string()),
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let refs = FileRefStore::new(&git_dir, format);
    if abort {
        return sequencer_abort(&git_dir, &worktree_root, format, "REVERT_HEAD");
    }
    let target = match positional.as_slice() {
        [target] => target.clone(),
        [] => return Err(GitError::Command("revert requires a commit".into())),
        _ => {
            return Err(GitError::Unsupported(
                "revert of multiple commits is not supported yet".into(),
            ));
        }
    };
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let revert = read_reused_commit(&git_dir, format, &target)?;
    let revert_oid =
        sley_rev::peel_to_commit(&db, format, &resolve_revision(&git_dir, format, &target)?)?;
    let head_oid = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("revert onto unborn HEAD is not supported".into()))?;
    // Reverse application: base is the commit, theirs is its parent.
    let base_map = stash_tree_entry_map(&db, format, &revert.tree)?;
    let theirs_map = match revert.parents.first() {
        Some(parent) => {
            let parent_tree = commit_tree_oid(&db, format, parent)?;
            stash_tree_entry_map(&db, format, &parent_tree)?
        }
        None => MergeTreeMap::new(),
    };
    let identity = commit_identity_from_env("COMMITTER")?;
    let author = commit_identity_from_env("AUTHOR")?;
    let subject = commit_subject(&revert.message);
    let message = format!(
        "Revert \"{subject}\"\n\nThis reverts commit {}.\n",
        revert_oid.to_hex()
    );
    finalize_replay(
        &git_dir,
        &common_git_dir,
        &worktree_root,
        format,
        &refs,
        &head_oid,
        ReplayPlan {
            base: base_map,
            theirs: theirs_map,
            theirs_label: format!(
                "parent of {} ({subject})",
                format_log_abbrev_oid(&revert_oid)
            ),
            new_parents: vec![head_oid],
            author,
            committer: identity,
            message: message.into_bytes(),
            state_file: "REVERT_HEAD",
            state_oid: revert_oid,
            reflog_message: format!("revert: {subject}").into_bytes(),
            conflict_error: format!(
                "error: could not revert {}... {subject}",
                &revert_oid.to_hex()[..7.min(revert_oid.to_hex().len())]
            ),
        },
    )
}

pub(crate) fn cmd_merge_base(args: &[String]) -> Result<()> {
    let mut all = false;
    let mut is_ancestor = false;
    let mut independent = false;
    let mut octopus = false;
    let mut fork_point = false;
    let mut revs = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            revs.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--all" | "-a" => all = true,
            "--no-all" => all = false,
            "--is-ancestor" => is_ancestor = true,
            "--independent" => independent = true,
            "--octopus" => octopus = true,
            "--fork-point" => fork_point = true,
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "merge-base currently supports --all, --is-ancestor, --independent, --octopus, --fork-point, and commit arguments; unsupported option {value}"
                )));
            }
            value => revs.push(value),
        }
    }
    if fork_point && !(revs.len() == 1 || revs.len() == 2) {
        return Err(GitError::Command(
            "merge-base --fork-point requires a ref and optional commit".into(),
        ));
    }
    if is_ancestor && revs.len() != 2 {
        return Err(GitError::Command(
            "merge-base currently requires exactly two commits".into(),
        ));
    }
    if independent && all {
        eprintln!("fatal: options '--independent' and '--all' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if independent && is_ancestor {
        eprintln!("error: options '--independent' and '--is-ancestor' cannot be used together");
        return Err(GitError::Exit(129));
    }
    if !fork_point && !octopus && !independent && revs.len() < 2 {
        return Err(GitError::Command(
            "merge-base currently requires at least two commits".into(),
        ));
    }
    if (octopus || independent) && revs.is_empty() {
        return Err(GitError::Command(
            "merge-base requires at least one commit for this mode".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    if fork_point {
        let commit = if let Some(commit) = revs.get(1) {
            let oid = resolve_revision(&git_dir, format, commit)?;
            sley_rev::peel_to_commit(&db, format, &oid)?
        } else {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            sley_rev::peel_to_commit(&db, format, &oid)?
        };
        if let Some(base) = merge_base_fork_point(&git_dir, format, &db, revs[0], &commit)? {
            println!("{base}");
            return Ok(());
        }
        return Err(GitError::Exit(1));
    }
    let mut commits = Vec::with_capacity(revs.len());
    for rev in &revs {
        let oid = resolve_revision(&git_dir, format, rev)?;
        commits.push(sley_rev::peel_to_commit(&db, format, &oid)?);
    }
    if is_ancestor {
        // Graph-accelerated reachability (generation-number pruning + parents from
        // the commit-graph) instead of walking every ancestor's object.
        if sley_rev::is_ancestor(&git_dir, format, &db, &commits[0], &commits[1])? {
            return Ok(());
        }
        return Err(GitError::Exit(1));
    }
    if independent {
        for commit in merge_base_independent(&db, format, &commits)? {
            println!("{commit}");
        }
        return Ok(());
    }
    let bases = if octopus {
        merge_bases_many(&db, format, &commits)?
    } else if commits.len() > 2 {
        merge_bases_default_many(&db, format, &commits)?
    } else {
        // Two-commit merge base via the commit-graph (parents + generation numbers
        // from the graph) rather than the object-reading ancestor walk.
        sley_rev::merge_bases(&git_dir, format, &db, &commits[0], &commits[1])?
    };
    if bases.is_empty() {
        return Err(GitError::Exit(1));
    }
    if all {
        for base in bases {
            println!("{base}");
        }
    } else {
        println!("{}", bases[0]);
    }
    Ok(())
}

pub(crate) fn merge_bases(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left: &ObjectId,
    right: &ObjectId,
) -> Result<Vec<ObjectId>> {
    let left_depths = ancestor_depths(db, format, left)?;
    let right_depths = ancestor_depths(db, format, right)?;
    let mut common = left_depths
        .keys()
        .filter(|oid| right_depths.contains_key(*oid))
        .cloned()
        .collect::<Vec<_>>();
    let candidates = common.clone();
    common = candidates
        .iter()
        .filter(|candidate| {
            !candidates.iter().any(|other| {
                other != *candidate
                    && left_depths.get(other).is_some_and(|other_depth| {
                        left_depths
                            .get(*candidate)
                            .is_some_and(|candidate_depth| other_depth < candidate_depth)
                    })
                    && right_depths.get(other).is_some_and(|other_depth| {
                        right_depths
                            .get(*candidate)
                            .is_some_and(|candidate_depth| other_depth < candidate_depth)
                    })
            })
        })
        .cloned()
        .collect();
    common.sort_by(|left_oid, right_oid| {
        let left_score = left_depths[left_oid] + right_depths[left_oid];
        let right_score = left_depths[right_oid] + right_depths[right_oid];
        left_score
            .cmp(&right_score)
            .then_with(|| left_depths[left_oid].cmp(&left_depths[right_oid]))
            .then_with(|| left_oid.to_hex().cmp(&right_oid.to_hex()))
    });
    Ok(common)
}

fn merge_bases_default_many(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let left_depths = ancestor_depths(db, format, &commits[0])?;
    let other_depths = commits
        .iter()
        .skip(1)
        .map(|commit| ancestor_depths(db, format, commit))
        .collect::<Result<Vec<_>>>()?;
    let mut common = left_depths
        .keys()
        .filter(|oid| other_depths.iter().any(|map| map.contains_key(*oid)))
        .cloned()
        .collect::<Vec<_>>();
    let candidates = common.clone();
    let candidate_depths = candidates
        .iter()
        .map(|candidate| Ok((candidate.clone(), ancestor_depths(db, format, candidate)?)))
        .collect::<Result<HashMap<_, _>>>()?;
    common.retain(|candidate| {
        !candidates.iter().any(|other| {
            other != candidate
                && candidate_depths
                    .get(other)
                    .is_some_and(|ancestors| ancestors.contains_key(candidate))
        })
    });
    common.sort_by(|left_oid, right_oid| {
        let left_other_depth = other_depths
            .iter()
            .filter_map(|map| map.get(left_oid))
            .min()
            .copied()
            .unwrap_or(usize::MAX);
        let right_other_depth = other_depths
            .iter()
            .filter_map(|map| map.get(right_oid))
            .min()
            .copied()
            .unwrap_or(usize::MAX);
        let left_score = left_depths[left_oid] + left_other_depth;
        let right_score = left_depths[right_oid] + right_other_depth;
        left_score
            .cmp(&right_score)
            .then_with(|| left_depths[left_oid].cmp(&left_depths[right_oid]))
            .then_with(|| left_oid.to_hex().cmp(&right_oid.to_hex()))
    });
    Ok(common)
}

fn merge_bases_many(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    if let [commit] = commits {
        return Ok(vec![*commit]);
    }
    let depths = commits
        .iter()
        .map(|commit| ancestor_depths(db, format, commit))
        .collect::<Result<Vec<_>>>()?;
    let mut common = depths[0]
        .keys()
        .filter(|oid| depths.iter().skip(1).all(|map| map.contains_key(*oid)))
        .cloned()
        .collect::<Vec<_>>();
    let candidates = common.clone();
    common = candidates
        .iter()
        .filter(|candidate| {
            !candidates.iter().any(|other| {
                other != *candidate
                    && depths.iter().all(|map| {
                        map.get(other).zip(map.get(*candidate)).is_some_and(
                            |(other_depth, candidate_depth)| other_depth < candidate_depth,
                        )
                    })
            })
        })
        .cloned()
        .collect();
    common.sort_by(|left_oid, right_oid| {
        let left_score = depths.iter().map(|map| map[left_oid]).sum::<usize>();
        let right_score = depths.iter().map(|map| map[right_oid]).sum::<usize>();
        left_score
            .cmp(&right_score)
            .then_with(|| {
                depths
                    .iter()
                    .map(|map| map[left_oid])
                    .cmp(depths.iter().map(|map| map[right_oid]))
            })
            .then_with(|| left_oid.to_hex().cmp(&right_oid.to_hex()))
    });
    Ok(common)
}

fn merge_base_independent(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for commit in commits {
        if seen.insert(commit) {
            unique.push(*commit);
        }
    }
    let depths = unique
        .iter()
        .map(|commit| ancestor_depths(db, format, commit))
        .collect::<Result<Vec<_>>>()?;
    let mut independent = Vec::new();
    for (idx, commit) in unique.iter().enumerate() {
        let reachable_from_other = depths
            .iter()
            .enumerate()
            .any(|(other_idx, ancestors)| other_idx != idx && ancestors.contains_key(commit));
        if !reachable_from_other {
            independent.push(*commit);
        }
    }
    Ok(independent)
}

fn merge_base_fork_point(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    ref_arg: &str,
    commit: &ObjectId,
) -> Result<Option<ObjectId>> {
    let Some(refname) = rev_parse_symbolic_full_name(git_dir, format, ref_arg)? else {
        return Ok(None);
    };
    let store = FileRefStore::new(git_dir, format);
    let reflog = store.read_reflog(&refname)?;
    if reflog.is_empty() {
        return Ok(None);
    }
    let commit_depths = ancestor_depths(db, format, commit)?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for entry in reflog {
        if commit_depths.contains_key(&entry.new_oid) && seen.insert(entry.new_oid) {
            candidates.push(entry.new_oid);
        }
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    let candidate_depths = candidates
        .iter()
        .map(|candidate| Ok((candidate.clone(), ancestor_depths(db, format, candidate)?)))
        .collect::<Result<HashMap<_, _>>>()?;
    let all_candidates = candidates.clone();
    candidates.retain(|candidate| {
        !all_candidates.iter().any(|other| {
            other != candidate
                && candidate_depths
                    .get(other)
                    .is_some_and(|ancestors| ancestors.contains_key(candidate))
        })
    });
    candidates.sort_by(|left, right| {
        commit_depths[left]
            .cmp(&commit_depths[right])
            .then_with(|| left.to_hex().cmp(&right.to_hex()))
    });
    Ok(candidates.into_iter().next())
}
