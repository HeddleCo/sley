use super::*;
use sley::plumbing::{sley_config, sley_core, sley_index, sley_refs, sley_rev, sley_worktree};

/// Render git merge's post-merge `--stat`/`--compact-summary` block.
///
/// git (`builtin/merge.c`) drives this from `show_diffstat`:
///   * `MERGE_SHOW_DIFFSTAT` → `DIFF_FORMAT_DIFFSTAT | DIFF_FORMAT_SUMMARY`,
///     i.e. the diffstat followed by the `create/delete mode`/`rename` summary
///     block;
///   * `MERGE_SHOW_COMPACTSUMMARY` → `DIFF_FORMAT_DIFFSTAT` with
///     `stat_with_summary`, folding the summary into the stat rows (no separate
///     block);
///   * off → nothing.
///
/// Rename detection is always on (git sets `DIFF_DETECT_RENAME`).
fn write_merge_result_diffstat(
    stdout: &mut io::Stdout,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    mode: MergeDiffstat,
) -> Result<()> {
    if mode == MergeDiffstat::Off {
        return Ok(());
    }
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    let compact = mode == MergeDiffstat::Compact;
    let stat_entries = collect_diff_stat_entries(&entries, db, None, false)?;
    write_diff_stat_materialized(
        stdout,
        &stat_entries,
        DiffStatOptions {
            compact_summary: compact,
            stat_count: None,
            color: false,
            quote_path_fully: true,
        },
    )?;
    // The default `--stat` mode appends a `DIFF_FORMAT_SUMMARY` block (the
    // ` create mode`/` delete mode`/` rename`/` mode change` lines). The
    // compact mode inlines that information into the stat rows instead, so it
    // emits no separate block.
    if !compact {
        for entry in &entries {
            write_diff_summary_entry(stdout, entry)?;
        }
    }
    Ok(())
}

/// Resolve git merge's effective `show_diffstat` value: an explicit CLI flag
/// wins, otherwise `merge.stat` config decides (`false`/`no`/`off` → off,
/// `compact` → compact, anything else / unset → the default full diffstat).
fn merge_diffstat_mode(options: &MergeOptions) -> MergeDiffstat {
    if let Some(mode) = options.diffstat {
        return mode;
    }
    let value = effective_config_with_overrides()
        .and_then(|config| config.get("merge", None, "stat").map(str::to_string));
    match value.as_deref() {
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "false" | "no" | "off" | "0" => MergeDiffstat::Off,
            "compact" => MergeDiffstat::Compact,
            _ => MergeDiffstat::Stat,
        },
        None => MergeDiffstat::Stat,
    }
}

struct MergeAttributeFavorResolver {
    matcher: Option<sley_worktree::StandardAttributeMatcher>,
}

impl MergeAttributeFavorResolver {
    fn from_worktree_root(worktree_root: &Path) -> Self {
        Self {
            matcher: sley_worktree::StandardAttributeMatcher::from_worktree_root(worktree_root)
                .ok(),
        }
    }

    fn favor_for_path(&self, path: &[u8]) -> sley_diff_merge::MergeFavor {
        match self.merge_attribute_for_path(path) {
            Some(sley_worktree::AttributeState::Value(value)) if value == b"union" => {
                sley_diff_merge::MergeFavor::Union
            }
            _ => sley_diff_merge::MergeFavor::None,
        }
    }

    fn is_binary_for_path(&self, path: &[u8]) -> bool {
        matches!(
            self.merge_attribute_for_path(path),
            Some(sley_worktree::AttributeState::Unset)
        )
    }

    fn merge_attribute_for_path(&self, path: &[u8]) -> Option<sley_worktree::AttributeState> {
        let Some(matcher) = self.matcher.as_ref() else {
            return None;
        };
        let requested = [b"merge".to_vec()];
        matcher
            .attributes_for_path(path, &requested, false)
            .into_iter()
            .next()
            .and_then(|check| check.state)
    }
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
    options: &MergeOptions,
) -> Result<ObjectId> {
    let message = prepare_merge_commit_message_for_commit_with_rollback(
        git_dir, format, head_oid, message, options,
    )?;
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let encoding = commit_encoding_header_from_config(git_dir);
    let signature = merge_commit_signature(
        git_dir,
        format,
        tree,
        vec![*head_oid, *other_oid],
        &author,
        &committer,
        &message,
        encoding.as_deref(),
        options,
    )?;
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![*head_oid, *other_oid],
            author,
            committer: committer.clone(),
            message,
            encoding,
            signature,
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
    commands::hooks::run_hook("reference-transaction", commands::hooks::HookRun::default())?;
    Ok(oid)
}

/// Commit + advance HEAD for `-s ours`. Identical to [`merge_commit_and_advance`]
/// except the reflog message names the `ours` strategy and uses the merge target
/// label (e.g. `merge main: Merge made by the 'ours' strategy.`), matching git's
/// `merge-ours` reflog exactly.
#[allow(clippy::too_many_arguments)]
fn merge_ours_commit_and_advance(
    git_dir: &Path,
    refs: &FileRefStore,
    format: ObjectFormat,
    head_oid: &ObjectId,
    other_oid: &ObjectId,
    tree: ObjectId,
    target_label: &str,
    message: Vec<u8>,
    options: &MergeOptions,
) -> Result<ObjectId> {
    let message = prepare_merge_commit_message_for_commit_with_rollback(
        git_dir, format, head_oid, message, options,
    )?;
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let encoding = commit_encoding_header_from_config(git_dir);
    let signature = merge_commit_signature(
        git_dir,
        format,
        tree,
        vec![*head_oid, *other_oid],
        &author,
        &committer,
        &message,
        encoding.as_deref(),
        options,
    )?;
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![*head_oid, *other_oid],
            author,
            committer: committer.clone(),
            message,
            encoding,
            signature,
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
            message: format!("merge {target_label}: Merge made by the 'ours' strategy.")
                .into_bytes(),
        }),
    });
    tx.commit()?;
    commands::hooks::run_hook("reference-transaction", commands::hooks::HookRun::default())?;
    Ok(oid)
}

fn merge_commit_signature(
    git_dir: &Path,
    format: ObjectFormat,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: &[u8],
    committer: &[u8],
    message: &[u8],
    encoding: Option<&[u8]>,
    options: &MergeOptions,
) -> Result<Option<Vec<u8>>> {
    if !options.gpg_sign {
        return Ok(None);
    }
    let config = read_repo_config(git_dir).ok();
    let unsigned = Commit {
        tree,
        parents,
        author: author.to_vec(),
        committer: committer.to_vec(),
        encoding: encoding.map(<[u8]>::to_vec),
        message: message.to_vec(),
    };
    let key =
        commands::signing::signing_key(config.as_ref(), options.gpg_sign_key.as_deref(), committer);
    commands::signing::sign_payload(config.as_ref(), &unsigned.write(), key.as_deref()).map(Some)
}

/// True when `ancestor` is reachable from `of` (an ancestor of, or equal to,
/// `of`) — git's `in_merge_bases` predicate over two commits.
fn is_ancestor_commit(
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    ancestor: &ObjectId,
    of: &ObjectId,
) -> Result<bool> {
    if ancestor == of {
        return Ok(true);
    }
    Ok(merge_bases(git_dir, db, format, ancestor, of)?
        .iter()
        .any(|base| base == ancestor))
}

/// git's `reduce_heads` over the named merge targets: drop any head already
/// reachable from HEAD or from another named head (a duplicate keeps only its
/// first occurrence), preserving command-line order. Used by the strategy
/// dispatch (one survivor ⇒ regular two-parent merge; ≥2 ⇒ octopus) and by the
/// octopus driver itself so both agree on the parent set.
fn reduce_merge_targets(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    refs: &FileRefStore,
    targets: &[String],
) -> Result<Vec<(String, ObjectId)>> {
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let head_oid = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
        Some(RefTarget::Direct(oid)) => Some(oid),
        None => None,
    };

    let mut heads = Vec::with_capacity(targets.len());
    for target in targets {
        let oid = peel_merge_target_to_commit(
            &db,
            format,
            resolve_merge_target_revision(git_dir, format, target)?,
        )?;
        heads.push((target.clone(), oid));
    }

    let is_ancestor =
        |db: &FileObjectDatabase, ancestor: &ObjectId, of: &ObjectId| -> Result<bool> {
            if ancestor == of {
                return Ok(true);
            }
            Ok(merge_bases(git_dir, db, format, ancestor, of)?
                .iter()
                .any(|base| base == ancestor))
        };
    let mut reduced: Vec<(String, ObjectId)> = Vec::new();
    'heads: for (index, (name, oid)) in heads.iter().enumerate() {
        if let Some(head_oid) = head_oid
            && is_ancestor(&db, oid, &head_oid)?
        {
            continue;
        }
        for (other_index, (_, other)) in heads.iter().enumerate() {
            if other_index == index {
                continue;
            }
            if oid == other {
                if other_index < index {
                    continue 'heads;
                }
                continue;
            }
            if is_ancestor(&db, oid, other)? {
                continue 'heads;
            }
        }
        reduced.push((name.clone(), *oid));
    }
    Ok(reduced)
}

/// `git merge <a> <b> [...]` — the octopus strategy. Mirrors upstream's
/// `git-merge-octopus`: iteratively three-way-merge each head onto the running
/// merged tree (MRT), fast-forwarding where possible, and refuse (exit 2) the
/// moment any pairwise step conflicts — an octopus merge must be trivially
/// clean. The final commit records HEAD plus every non-redundant head as
/// parents, in command-line order.
#[allow(clippy::too_many_arguments)]
fn merge_octopus(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    refs: &FileRefStore,
    targets: &[String],
    options: &MergeOptions,
) -> Result<()> {
    let head_oid = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => oid,
            _ => {
                return Err(GitError::Unsupported(
                    "octopus merge into an unborn branch is not supported".into(),
                ));
            }
        },
        Some(RefTarget::Direct(oid)) => oid,
        None => {
            return Err(GitError::Command("HEAD is not a valid revision".into()));
        }
    };
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);

    // git-merge-octopus's `git diff-index --quiet --cached HEAD` guard: a staged
    // change vs HEAD makes the index an unclean octopus base. Refuse (exit 2)
    // before writing any merge state.
    let status = crate::collect_short_status(worktree_root, git_dir, format)?;
    if let Some(entry) = status
        .iter()
        .find(|e| e.index != b' ' && e.index != b'?' && e.index != b'!')
    {
        eprintln!(
            "Error: Your local changes to the following files would be overwritten by merge\n    {}",
            String::from_utf8_lossy(&entry.path)
        );
        return Err(GitError::Exit(2));
    }

    let reduced = reduce_merge_targets(git_dir, common_git_dir, format, refs, targets)?;
    if reduced.is_empty() {
        if !options.quiet {
            println!("Already up to date.");
        }
        return Ok(());
    }

    // Iterative octopus: MRC tracks the commits the running tree stands for.
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let mut merged_map = sley_diff_merge::flatten_tree(&db, format, &head_tree)?;
    let mut merged_commits = vec![head_oid];
    let mut non_ff = false;
    // git-merge-octopus allows only the LAST head to leave a hand-resolvable
    // conflict; if a conflict occurs and another head still remains, the octopus
    // gives up entirely.
    let mut octopus_failure = false;
    for (name, oid) in &reduced {
        // A prior pairwise step conflicted but more heads remained: git's
        // "Should not be doing an octopus" bail (exit 2, no state left behind).
        if octopus_failure {
            eprintln!("Automated merge did not work.");
            eprintln!("Should not be doing an octopus.");
            eprintln!("fatal: merge program failed");
            return Err(GitError::Exit(2));
        }
        let mut base_args = vec![*oid];
        base_args.extend(merged_commits.iter().copied());
        let common = merge_bases_default_many(common_git_dir, &db, format, &base_args)?;
        if common.len() == 1 && common[0] == *oid {
            // Already covered by the merges performed so far. git's octopus
            // prints "Already up to date with <name>" and moves on.
            if !options.quiet {
                println!("Already up to date with {name}");
            }
            continue;
        }
        if !non_ff
            && merged_commits.len() == 1
            && common.len() == 1
            && common[0] == merged_commits[0]
        {
            // Fast-forward the running state to this head (git-merge-octopus's
            // "Fast-forwarding to: <name>").
            if !options.quiet {
                println!("Fast-forwarding to: {name}");
            }
            let tree = commit_tree_oid(&db, format, oid)?;
            merged_map = sley_diff_merge::flatten_tree(&db, format, &tree)?;
            merged_commits = vec![*oid];
            continue;
        }
        if common.is_empty() {
            eprintln!("Unable to find common commit with {name}");
            return Err(GitError::Exit(2));
        }
        // `--ff-only`: a real (non-fast-forward) octopus step is needed, which
        // an ff-only merge cannot satisfy. git refuses before merging.
        if options.ff_only() {
            eprintln!("fatal: Not possible to fast-forward, aborting.");
            return Err(GitError::Exit(128));
        }
        non_ff = true;
        // git-merge-octopus's "Trying simple merge with <name>" line precedes
        // each non-fast-forward pairwise step.
        if !options.quiet {
            println!("Trying simple merge with {name}");
        }
        let base_map = virtual_ancestor_entry_map(&db, format, &common, common_git_dir)?;
        let theirs_tree = commit_tree_oid(&db, format, oid)?;
        let theirs_map = sley_diff_merge::flatten_tree(&db, format, &theirs_tree)?;
        let (results, conflicts) = three_way_merge_trees_with_favor(
            &db,
            format,
            &base_map,
            &merged_map,
            &theirs_map,
            "HEAD",
            name,
            options.favor,
        )?;
        if !conflicts.is_empty() {
            // git-merge-octopus: a conflict sets OCTOPUS_FAILURE but the loop
            // continues — only the LAST head may conflict (hand-resolvable). If
            // another head remains, the next iteration's guard above bails with
            // "Should not be doing an octopus". Don't advance the running state.
            octopus_failure = true;
            continue;
        }
        let mut next: MergeTreeMap = BTreeMap::new();
        for (path, result) in results {
            if let MergePathResult::Resolved(Some(entry)) = result {
                next.insert(path, entry);
            }
        }
        merged_map = next;
        merged_commits.push(*oid);
    }

    // The LAST head conflicted (octopus allows exactly one hand-resolvable
    // conflict). sley's octopus does not model materialising that conflicted
    // state, so report the failure and leave the tree untouched (exit 2), as
    // git's octopus does for an unresolvable final step.
    if octopus_failure {
        eprintln!("Automated merge did not work.");
        eprintln!("Should not be doing an octopus.");
        eprintln!("fatal: merge program failed");
        return Err(GitError::Exit(2));
    }

    if !non_ff && merged_commits.len() == 1 && reduced.len() == 1 {
        // Degenerated to a plain fast-forward.
        let new_oid = merged_commits[0];
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: Some(RefTarget::Direct(head_oid)),
            new: RefTarget::Direct(new_oid),
            reflog: Some(ReflogEntry {
                old_oid: head_oid,
                new_oid,
                committer: commit_identity_from_env("COMMITTER")?,
                message: merge_reflog_message(&reduced[0].0, "Fast-forward"),
            }),
        });
        tx.commit()?;
        reset_index_and_worktree_to_commit_for_merge(
            worktree_root,
            git_dir,
            format,
            &new_oid,
            options.recurse_submodules,
        )?;
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "Updating {}..{}",
                format_log_abbrev_oid(&head_oid),
                format_log_abbrev_oid(&new_oid)
            )?;
            writeln!(stdout, "Fast-forward")?;
            let new_tree = commit_tree_oid(&db, format, &new_oid)?;
            write_merge_result_diffstat(
                &mut stdout,
                &db,
                format,
                &head_tree,
                &new_tree,
                merge_diffstat_mode(options),
            )?;
            stdout.flush()?;
        }
        return Ok(());
    }

    // Build the merged tree via a temporary stage-0 index, mirroring the
    // two-parent clean path above.
    let mut entries = Vec::new();
    for (path, (mode, oid)) in &merged_map {
        entries.push(merge_index_entry(path, *mode, *oid, 0));
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
    let merged_tree = sley_worktree::write_tree_from_index(git_dir, format)?;

    let message = build_merge_message(refs, git_dir, &db, format, options, &head_oid, &reduced)?;

    // Materialize the merged result into the worktree, touching only paths that
    // differ from HEAD (preserve untouched local mods, as in the two-parent path).
    let head_map = &sley_diff_merge::flatten_tree(&db, format, &head_tree)?;
    let sync_octopus_worktree = || -> Result<()> {
        for (path, entry) in &merged_map {
            if head_map.get(path) == Some(entry) {
                continue;
            }
            let (mode, oid) = entry;
            let content = merge_worktree_content(&db, *mode, oid)?;
            merge_write_worktree_file(worktree_root, path, &content, *mode)?;
        }
        for path in head_map.keys() {
            if !merged_map.contains_key(path) {
                merge_remove_worktree_file(worktree_root, path)?;
            }
        }
        Ok(())
    };

    // `--squash`: stage the merged result + write SQUASH_MSG, record NO merge.
    if options.squash {
        sync_octopus_worktree()?;
        refresh_merged_index_stat(git_dir, worktree_root, format)?;
        let other_oids: Vec<ObjectId> = reduced.iter().map(|(_, oid)| *oid).collect();
        write_squash_message_multi(git_dir, &db, format, &head_oid, &other_oids)?;
        if !options.quiet {
            println!("Squash commit -- not updating HEAD");
        }
        commands::hooks::run_hook_l("post-merge", &["1"])?;
        return Ok(());
    }

    // `--no-commit`: stage the merged result, record MERGE_HEAD (every merged
    // head) + MERGE_MSG, but do not create the commit or advance HEAD.
    if options.no_commit {
        sync_octopus_worktree()?;
        refresh_merged_index_stat(git_dir, worktree_root, format)?;
        let other_oids = reduced.iter().map(|(_, oid)| *oid).collect::<Vec<_>>();
        write_merge_state(
            git_dir,
            &other_oids,
            merge_msg_file_contents(&message),
            options,
            Some(&head_oid),
        )?;
        if !options.quiet {
            println!("Automatic merge went well; stopped before committing as requested");
        }
        return Ok(());
    }

    if !options.quiet {
        let mut stdout = io::stdout();
        writeln!(stdout, "Merge made by the 'octopus' strategy.")?;
        write_merge_result_diffstat(
            &mut stdout,
            &db,
            format,
            &head_tree,
            &merged_tree,
            merge_diffstat_mode(options),
        )?;
        stdout.flush()?;
    }

    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let encoding = commit_encoding_header_from_config(git_dir);
    let mut write_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    // git's `collect_parents`/`reduce_parents`: the parent set is the reduced
    // independent heads, with HEAD prepended only when HEAD was NOT subsumed
    // (i.e. it is not an ancestor of any merged head) OR `--no-ff` forces it in.
    // `reduced` already excludes any head reachable from HEAD, so HEAD is
    // "subsumed" exactly when it is an ancestor of some reduced head.
    let head_subsumed = reduced.iter().any(|(_, oid)| {
        oid == &head_oid
            || is_ancestor_commit(&db, git_dir, format, &head_oid, oid).unwrap_or(false)
    });
    let mut parents: Vec<ObjectId> = Vec::with_capacity(reduced.len() + 1);
    if !head_subsumed || options.no_ff() {
        parents.push(head_oid);
    }
    parents.extend(reduced.iter().map(|(_, oid)| *oid));
    let merged_oid = sley_sequencer::create_commit(
        &mut write_db,
        sley_sequencer::CommitCreate {
            tree: merged_tree,
            parents,
            author,
            committer: committer.clone(),
            message: prepare_merge_commit_message(git_dir, &message, options)?,
            encoding,
            signature: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: Some(RefTarget::Direct(head_oid)),
        new: RefTarget::Direct(merged_oid),
        reflog: Some(ReflogEntry {
            old_oid: head_oid,
            new_oid: merged_oid,
            committer,
            message: "merge: Merge made by the 'octopus' strategy.".into(),
        }),
    });
    tx.commit()?;
    sley_worktree::reset_index_and_worktree_to_commit_with_process_filter_metadata(
        worktree_root,
        git_dir,
        format,
        &merged_oid,
        Some(vec![("treeish".to_string(), merged_oid.to_hex())]),
    )?;
    Ok(())
}

/// After a clean `--squash`/`--no-commit` merge has materialized the merged
/// result into the worktree, record the on-disk stat for the stage-0 entries our
/// staging wrote with a zeroed stat. git's merge checks the merged result out and
/// `fill_stat_cache_info` records each entry's stat; without it `git diff-files`
/// (and `git status`) report every merged path as modified. Only zero-stat,
/// non-gitlink stage-0 entries whose worktree file exists are touched; conflict
/// stages never reach this path.
fn refresh_merged_index_stat(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(());
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let mut changed = false;
    for entry in &mut index.entries {
        if (entry.flags >> 12) & 0x3 != 0
            || sley_index::is_gitlink(entry.mode)
            || entry.mtime_seconds != 0
            || entry.ctime_seconds != 0
        {
            continue;
        }
        if let Ok(rel) = std::str::from_utf8(entry.path.as_bytes())
            && let Ok(metadata) = fs::symlink_metadata(worktree_root.join(rel))
        {
            sley_worktree::fill_index_entry_stat_cache(entry, &metadata);
            changed = true;
        }
    }
    if changed {
        fs::write(&index_path, index.write(format)?)?;
    }
    Ok(())
}

/// Build and write `.git/SQUASH_MSG` for a `--squash` merge of `other` onto
/// `head`, mirroring git's `squash_message` (builtin/merge.c): the literal
/// header `Squashed commit of the following:` then, for each commit reachable
/// from `other` but not `head` (newest first by commit date), a blank line,
/// `commit <full-oid>`, and the commit rendered in `git log` MEDIUM format.
fn write_squash_message(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head: &ObjectId,
    other: &ObjectId,
) -> Result<()> {
    write_squash_message_multi(git_dir, db, format, head, std::slice::from_ref(other))
}

/// `--squash` SQUASH_MSG for a merge of one or more heads (octopus): the
/// `^HEAD <other>...` range rendered as git's `squash_message`. Mirrors
/// `write_squash_message` but seeds the walk from every merged head.
fn write_squash_message_multi(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head: &ObjectId,
    others: &[ObjectId],
) -> Result<()> {
    // Mark HEAD's ancestors uninteresting, then collect every `other`'s ancestors
    // that are not among them (the `^HEAD other...` range).
    let uninteresting = sley_rev::ancestor_depths(git_dir, format, db, head)?;
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = others.iter().cloned().collect();
    while let Some(oid) = pending.pop_front() {
        if uninteresting.contains_key(&oid) || !seen.insert(oid.clone()) {
            continue;
        }
        let record = read_rev_list_commit_record(db, format, oid.clone())?;
        for parent in &record.parents {
            if !uninteresting.contains_key(parent) {
                pending.push_back(parent.clone());
            }
        }
        records.push(record);
    }
    // `git log` default order is reverse-chronological by commit date; ties keep
    // a stable order (children before parents, which the collection preserves).
    records.sort_by(|left, right| {
        let left_time = commit_identity_timestamp_i64(&left.commit.committer).unwrap_or(0);
        let right_time = commit_identity_timestamp_i64(&right.commit.committer).unwrap_or(0);
        right_time.cmp(&left_time)
    });

    let mut out = String::from("Squashed commit of the following:\n");
    for record in &records {
        out.push('\n');
        out.push_str(&format!("commit {}\n", record.oid));
        out.push_str(&format!(
            "Author: {}\n",
            commit_author_identity(&record.commit.author)
        ));
        out.push_str(&format!(
            "Date:   {}\n",
            commit_identity_date(&record.commit.author, &DateMode::Default)
        ));
        out.push('\n');
        for line in String::from_utf8_lossy(&record.commit.message).lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&format!("    {line}\n"));
            }
        }
    }
    fs::write(git_dir.join("SQUASH_MSG"), out)?;
    Ok(())
}

/// git's `merge_name` ref classification: how a merge target dwims to a ref,
/// driving both the title noun ("branch"/"tag"/…) and which `print_joined`
/// group a head lands in. Precedence follows `ref_rev_parse_rules`
/// (tags before heads), so a tag wins a name it shares with a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeRefKind {
    Branch,
    Tag,
    RemoteBranch,
    Commit,
}

/// Classify a single merge target into its `MergeRefKind` (git's `merge_name`).
fn classify_merge_target(refs: &FileRefStore, target: &str) -> Result<MergeRefKind> {
    let exists = |name: &str| -> Result<bool> { Ok(refs.read_ref(name)?.is_some()) };
    if exists(&format!("refs/tags/{target}"))? {
        Ok(MergeRefKind::Tag)
    } else if exists(&format!("refs/heads/{target}"))? {
        Ok(MergeRefKind::Branch)
    } else if exists(&format!("refs/remotes/{target}"))? {
        Ok(MergeRefKind::RemoteBranch)
    } else {
        Ok(MergeRefKind::Commit)
    }
}

fn classify_merge_target_for_message(
    refs: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    target: &str,
) -> Result<MergeRefKind> {
    let kind = classify_merge_target(refs, target)?;
    if kind != MergeRefKind::Commit {
        return Ok(kind);
    }
    if let Ok(oid) = resolve_revision(git_dir, format, target)
        && let Ok(object) = db.read_object(&oid)
        && object.object_type == ObjectType::Tag
    {
        return Ok(MergeRefKind::Tag);
    }
    Ok(kind)
}

/// git's `print_joined`: render a same-kind name list as
/// `<singular>'a'` (one) or `<plural>'a', 'b' and 'c'` (many).
fn print_joined(singular: &str, plural: &str, names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("{singular}'{one}'"),
        [rest @ .., last] => {
            let head = rest
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{plural}{head} and '{last}'")
        }
    }
}

fn print_joined_early_branches(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("branch '{one}' (early part)"),
        [rest @ .., last] => {
            let head = rest
                .iter()
                .map(|name| format!("'{name}' (early part)"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("branches {head} and '{last}' (early part)")
        }
    }
}

fn merge_target_early_branch(refs: &FileRefStore, target: &str) -> Result<Option<String>> {
    let Some(split) = target.find(['~', '^']) else {
        return Ok(None);
    };
    let branch = &target[..split];
    if branch.is_empty() {
        return Ok(None);
    }
    if refs.read_ref(&format!("refs/heads/{branch}"))?.is_some() {
        Ok(Some(branch.to_string()))
    } else {
        Ok(None)
    }
}

/// git's `merge.suppressDest` default: omit the ` into <branch>` title suffix
/// when the current branch is `main` or `master` (the built-in patterns).
fn merge_dest_suppressed(branch: &str) -> bool {
    merge_dest_suppressed_by_config(branch)
}

fn merge_dest_suppressed_by_config(branch: &str) -> bool {
    let Some(config) = effective_config_with_overrides() else {
        return branch == "main" || branch == "master";
    };
    let patterns: Vec<&str> = config
        .sections
        .iter()
        .filter(|section| section.name.eq_ignore_ascii_case("merge"))
        .filter(|section| section.subsection.is_none())
        .flat_map(|section| {
            section
                .entries
                .iter()
                .filter(|entry| entry.key.eq_ignore_ascii_case("suppressDest"))
                .map(|entry| entry.value.as_deref().unwrap_or(""))
        })
        .collect();
    if patterns.is_empty() {
        return branch == "main" || branch == "master";
    }
    patterns
        .iter()
        .any(|pattern| !pattern.is_empty() && glob_match_simple(pattern, branch))
}

fn glob_match_simple(pattern: &str, text: &str) -> bool {
    fn inner(pat: &[u8], text: &[u8]) -> bool {
        if pat.is_empty() {
            return text.is_empty();
        }
        match pat[0] {
            b'*' => inner(&pat[1..], text) || (!text.is_empty() && inner(pat, &text[1..])),
            b'?' => !text.is_empty() && inner(&pat[1..], &text[1..]),
            b'[' => {
                let Some(end) = pat.iter().position(|byte| *byte == b']') else {
                    return !text.is_empty() && pat[0] == text[0] && inner(&pat[1..], &text[1..]);
                };
                if text.is_empty() {
                    return false;
                }
                let class = &pat[1..end];
                class.contains(&text[0]) && inner(&pat[end + 1..], &text[1..])
            }
            c => !text.is_empty() && c == text[0] && inner(&pat[1..], &text[1..]),
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// The default merge commit subject (git's `fmt_merge_msg_title`): group the
/// merged heads by ref-kind, render each group via `print_joined`, and append
/// ` into <branch>` unless the destination is suppressed. Both the two-parent
/// and octopus paths route through this single function so the whole class of
/// merge-message cells stays git-exact.
fn merge_message_title(
    refs: &FileRefStore,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    targets: &[String],
    into_name: Option<&str>,
) -> Result<String> {
    // FETCH_HEAD merges keep their fetch-record-derived description and never
    // gain an `into` suffix (git's autogenerated-from-FETCH_HEAD path).
    if targets.len() == 1 && targets[0] == "FETCH_HEAD" {
        return Ok(fetch_head_merge_record(git_dir, format)
            .map(|record| format!("Merge {}", record.description))
            .unwrap_or_else(|_| format!("Merge commit '{}'", targets[0])));
    }

    let mut branches = Vec::new();
    let mut early_branches = Vec::new();
    let mut tags = Vec::new();
    let mut remotes = Vec::new();
    let mut commits = Vec::new();
    for target in targets {
        let target_name = merge_message_target_name(git_dir, format, target);
        if let Some(branch) = merge_target_early_branch(refs, &target_name)? {
            early_branches.push(branch);
            continue;
        }
        match classify_merge_target_for_message(refs, db, git_dir, format, &target_name)? {
            MergeRefKind::Branch => branches.push(target_name),
            MergeRefKind::Tag => tags.push(target_name),
            MergeRefKind::RemoteBranch => remotes.push(target_name),
            MergeRefKind::Commit => commits.push(target_name),
        }
    }

    let mut title = String::from("Merge ");
    let mut subsep = "";
    if !early_branches.is_empty() {
        title.push_str(&print_joined_early_branches(&early_branches));
        subsep = ", ";
    }
    for (singular, plural, list) in [
        ("branch ", "branches ", &branches),
        (
            "remote-tracking branch ",
            "remote-tracking branches ",
            &remotes,
        ),
        ("tag ", "tags ", &tags),
        ("commit ", "commits ", &commits),
    ] {
        if list.is_empty() {
            continue;
        }
        title.push_str(subsep);
        subsep = ", ";
        title.push_str(&print_joined(singular, plural, list));
    }

    let current_branch = into_name
        .map(str::to_string)
        .or_else(|| current_branch_short_name(refs).ok().flatten())
        .unwrap_or_else(|| "HEAD".to_string());
    if !merge_dest_suppressed(&current_branch) {
        title.push_str(&format!(" into {current_branch}"));
    }
    Ok(title)
}

fn merge_message_target_name(git_dir: &Path, format: ObjectFormat, target: &str) -> String {
    if !target.contains("@{") {
        return target.to_string();
    }
    match sley_rev::resolve_revision_symbolic_full_name(git_dir, format, target) {
        Ok(Some(refname)) => refname
            .strip_prefix("refs/remotes/")
            .or_else(|| refname.strip_prefix("refs/heads/"))
            .or_else(|| refname.strip_prefix("refs/tags/"))
            .unwrap_or(&refname)
            .to_string(),
        _ => target.to_string(),
    }
}

/// git's `merge_name` source descriptor for a single head, as it appears in the
/// `--log` shortlog header (`* tag 'c3':`). Same noun as the title, singular.
fn merge_log_origin_name(kind: MergeRefKind, target: &str) -> String {
    match kind {
        MergeRefKind::Branch => format!("branch '{target}'"),
        MergeRefKind::Tag => format!("tag '{target}'"),
        MergeRefKind::RemoteBranch => format!("remote-tracking branch '{target}'"),
        MergeRefKind::Commit => format!("commit '{target}'"),
    }
}

/// git's `--log` / `merge.log` shortlog body (`fmt-merge-msg.c shortlog`): for
/// each merged head, list the non-merge commits reachable from it but not from
/// HEAD, newest first, capped at `limit`. Renders `\n* <origin>:\n  <subject>\n`
/// (or `* <origin>: (N commits)` + `  ...` when the count exceeds the cap).
fn merge_log_shortlog(
    refs: &FileRefStore,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head_oid: &ObjectId,
    targets: &[(String, ObjectId)],
    limit: usize,
) -> Result<String> {
    let mut out = String::new();
    let head_reachable: std::collections::HashSet<ObjectId> =
        sley_rev::walk_commits(db, format, [*head_oid])?
            .into_iter()
            .map(|record| record.oid)
            .collect();
    for (name, oid) in targets {
        let kind = classify_merge_target_for_message(refs, db, git_dir, format, name)?;
        let origin = merge_log_origin_name(kind, name);
        // Commits reachable from the head but not from HEAD — git's revision
        // walk with `^HEAD <ref>`. Sort newest-first by committer time (git's
        // default commit-date order) and skip merges (`shortlog` lists only the
        // non-merge tip subjects).
        let mut walked: Vec<sley_rev::CommitRecord> = sley_rev::walk_commits(db, format, [*oid])?
            .into_iter()
            .filter(|record| !head_reachable.contains(&record.oid))
            .filter(|record| record.parents.len() <= 1)
            .collect();
        walked.sort_by(|a, b| {
            let ta = a
                .commit
                .committer_signature()
                .map(|s| s.time.seconds)
                .unwrap_or(0);
            let tb = b
                .commit
                .committer_signature()
                .map(|s| s.time.seconds)
                .unwrap_or(0);
            tb.cmp(&ta)
                .then_with(|| b.oid.to_hex().cmp(&a.oid.to_hex()))
        });
        let count = walked.len();
        let mut subjects = Vec::new();
        for record in walked.iter().take(limit + 1) {
            let subject = commit_subject(&record.commit.message);
            let subject = subject.trim().to_string();
            if subject.is_empty() {
                subjects.push(record.oid.to_hex());
            } else {
                subjects.push(subject);
            }
        }
        if count > limit {
            out.push_str(&format!("\n* {origin}: ({count} commits)\n"));
        } else {
            out.push_str(&format!("\n* {origin}:\n"));
        }
        for (i, subject) in subjects.iter().enumerate() {
            if i >= limit {
                out.push_str("  ...\n");
            } else {
                out.push_str(&format!("  {subject}\n"));
            }
        }
    }
    Ok(out)
}

fn complete_line_bytes(mut value: Vec<u8>) -> Vec<u8> {
    if !value.is_empty() && !value.ends_with(b"\n") {
        value.push(b'\n');
    }
    value
}

/// Build the full merge commit message git would write to `.git/MERGE_MSG`:
/// the title (auto-generated unless `-m` pins it) plus the `--log` / `merge.log`
/// shortlog body when `shortlog_len` is non-zero. This is the single producer
/// every finish path (two-parent / octopus / squash) shares so the message
/// class stays git-exact.
#[allow(clippy::too_many_arguments)]
fn build_merge_message(
    refs: &FileRefStore,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    options: &MergeOptions,
    head_oid: &ObjectId,
    targets: &[(String, ObjectId)],
) -> Result<Vec<u8>> {
    let names: Vec<String> = targets.iter().map(|(name, _)| name.clone()).collect();
    // Message source precedence (git): -F file, then -m, else the autogenerated
    // title. A user-supplied message (file or -m) suppresses the auto title.
    let mut message = if let Some(path) = &options.message_file {
        fs::read(path)?
    } else {
        match &options.message {
            Some(m) => argv_bytes_from_string(m),
            None => merge_message_title(
                refs,
                git_dir,
                db,
                format,
                &names,
                options.into_name.as_deref(),
            )?
            .into_bytes(),
        }
    };
    append_merge_target_tag_messages(&mut message, db, git_dir, format, &names)?;
    if let Some(limit) = options.shortlog_len
        && limit > 0
    {
        // git's `strbuf_complete_line`: the title is terminated with a newline
        // before the shortlog (which itself opens with a blank line), giving the
        // blank-line separator between an `-m` subject and the `* <ref>:` body.
        message = complete_line_bytes(message);
        let body = merge_log_shortlog(refs, git_dir, db, format, head_oid, targets, limit)?;
        message.extend_from_slice(body.as_bytes());
    }
    Ok(message)
}

fn merge_msg_file_contents(message: &[u8]) -> Vec<u8> {
    complete_line_bytes(message.to_vec())
}

fn append_merge_target_tag_messages(
    out: &mut Vec<u8>,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    targets: &[String],
) -> Result<()> {
    let mut blocks = Vec::new();
    for target in targets {
        let Ok(oid) = resolve_revision(git_dir, format, target) else {
            continue;
        };
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Tag {
            continue;
        }
        let tag = Tag::parse_ref(format, &object.body)?;
        let signature_kind = tag_signature_kind_local(tag.message).map(|(_, kind)| kind);
        let mut block = complete_line_string(
            String::from_utf8_lossy(fmt_tag_message_without_signature(tag.message)).into_owned(),
        );
        if let Some(kind) = signature_kind {
            append_synthetic_signature_note(&mut block, kind);
        }
        blocks.push(block);
    }
    if blocks.is_empty() {
        return Ok(());
    }
    append_blank_separator_bytes(out);
    for (idx, block) in blocks.iter().enumerate() {
        if idx > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(block.as_bytes());
    }
    Ok(())
}

/// git's `fast_forward` tri-state (builtin/merge.c): `FF_ALLOW` (the default —
/// fast-forward when possible, else make a merge commit), `FF_NO` (`--no-ff`:
/// always create a merge commit), `FF_ONLY` (`--ff-only`: refuse anything that
/// is not a fast-forward). `merge.ff` config seeds the default; CLI `--ff` /
/// `--no-ff` / `--ff-only` override it regardless of order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FastForward {
    Allow,
    No,
    Only,
}

struct MergeOptions {
    message: Option<String>,
    /// `None` until a CLI flag sets it; `merge.ff` config then seeds the default
    /// in [`apply_merge_config_defaults`]. Resolved to a concrete value before
    /// the merge runs.
    fast_forward: Option<FastForward>,
    no_commit: bool,
    quiet: bool,
    /// `--log[=N]` shortlog length. `None` means no CLI choice (the `merge.log` /
    /// `merge.summary` config decides); `Some(0)` is `--no-log`; `Some(n)` is the
    /// requested cap. Mirrors git's `shortlog_len` (default `DEFAULT_MERGE_LOG_LEN`
    /// = 20 when the config turns it on as a bool).
    shortlog_len: Option<usize>,
    /// `-X ours` / `-X theirs` conflict favouring for textual conflicts.
    favor: sley_diff_merge::MergeFavor,
    /// `--allow-unrelated-histories`: merge two branches with no common ancestor
    /// using the empty tree as the virtual base (git refuses by default).
    allow_unrelated_histories: bool,
    /// Diffstat display mode after a completed merge. Mirrors git's
    /// `show_diffstat` int driven by `-n`/`--stat`/`--summary`/
    /// `--compact-summary` and the `merge.stat` config. `None` means the field
    /// has not been set from the command line, so the `merge.stat` config still
    /// gets to decide; `Some(_)` is an explicit CLI choice that wins.
    diffstat: Option<MergeDiffstat>,
    /// `-s ours`: the merge keeps HEAD's tree verbatim and records the other
    /// commit only as a second parent (git's `merge-ours` strategy, which has
    /// `NO_FAST_FORWARD | NO_TRIVIAL`). Other strategies (`recursive`/`ort`)
    /// use the 3-way engine and leave this `false`.
    ours_strategy: bool,
    /// An explicit two-head strategy (`-s recursive` / `-s ort`) was requested.
    /// Multiple heads with such a strategy do not fall back to octopus.
    explicit_twohead_strategy: bool,
    /// `-s resolve`: handled by the same internal two-head engine, but its
    /// porcelain output names the historical resolve strategy.
    resolve_strategy: bool,
    /// `-s subtree`: the ancestry-only up-to-date case needs no tree shifting
    /// and is handled natively. Non-trivial subtree merges remain unsupported
    /// until the engine can apply the required prefix transformation.
    subtree_strategy: bool,
    /// `--squash`: stage the merged result and write `.git/SQUASH_MSG`, but do
    /// NOT create a merge commit or advance HEAD (git's `squash`). Implies
    /// `--no-commit`-like behaviour and is incompatible with `--commit`.
    squash: bool,
    /// `--cleanup=<mode>` / `commit.cleanup` config. `None` resolves to git's
    /// default for the (no-)editor case in [`resolve_merge_cleanup_mode`].
    cleanup: Option<CommitCleanupMode>,
    /// `-F`/`--file <path>` message source (read verbatim, then cleaned per the
    /// cleanup mode). Wins over the autogenerated title; `-m` and `-F` together
    /// is rejected by git but the tests never combine them.
    message_file: Option<String>,
    /// `-e`/`--edit` / `--no-edit`: whether the message goes through an editor.
    /// `--no-edit` uses the autogenerated message as-is; `--edit` writes
    /// `.git/MERGE_MSG` and launches the configured editor before committing.
    edit: Option<bool>,
    /// `--autostash` / `merge.autoStash`: stash tracked local work before the
    /// merge, then apply it after a completed merge or save/apply it from the
    /// in-progress merge state.
    autostash: Option<bool>,
    /// `--into-name=<name>`: override the destination name used in the generated
    /// merge message title.
    into_name: Option<String>,
    /// Move populated submodule worktrees when gitlink entries change.
    recurse_submodules: bool,
    /// `--rerere-autoupdate` / `--no-rerere-autoupdate`.
    rerere_autoupdate: Option<bool>,
    gpg_sign: bool,
    gpg_sign_key: Option<String>,
    signoff: bool,
    no_verify: bool,
}

/// git `merge.c`'s `show_diffstat` tri-state: off (`-n`/`--no-stat`),
/// the default `--stat` (diffstat + `create/delete mode` summary block), or
/// `--compact-summary` (diffstat with the summary folded into the rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeDiffstat {
    Off,
    Stat,
    Compact,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            message: None,
            fast_forward: None,
            no_commit: false,
            quiet: false,
            shortlog_len: None,
            favor: sley_diff_merge::MergeFavor::None,
            allow_unrelated_histories: false,
            diffstat: None,
            ours_strategy: false,
            explicit_twohead_strategy: false,
            resolve_strategy: false,
            subtree_strategy: false,
            squash: false,
            cleanup: None,
            message_file: None,
            edit: None,
            autostash: None,
            into_name: None,
            recurse_submodules: false,
            rerere_autoupdate: None,
            gpg_sign: false,
            gpg_sign_key: None,
            signoff: false,
            no_verify: false,
        }
    }
}

fn prepare_merge_commit_message_for_commit(
    git_dir: &Path,
    message: Vec<u8>,
    options: &MergeOptions,
) -> Result<Vec<u8>> {
    let mut message = if options.signoff {
        commands::replay::append_signoff_before_comments(message, &commit_signoff_from_env()?)
    } else {
        message
    };
    let editmsg = git_dir.join("COMMIT_EDITMSG");
    if !options.no_verify {
        commands::hooks::run_hook("pre-merge-commit", commands::hooks::HookRun::default())?;
    }
    fs::write(&editmsg, &message)?;
    commands::commit::run_prepare_commit_msg_hook(
        &editmsg,
        commands::commit::PrepareCommitMsgSource::Merge,
        Vec::new(),
        options.edit != Some(true),
    )?;
    if !options.no_verify {
        let editmsg_arg = editmsg.to_string_lossy().into_owned();
        commands::hooks::run_hook_l("commit-msg", &[editmsg_arg.as_str()])?;
    }
    message = fs::read(&editmsg)?;
    Ok(message)
}

fn prepare_merge_commit_message_for_commit_with_rollback(
    git_dir: &Path,
    format: ObjectFormat,
    head_oid: &ObjectId,
    message: Vec<u8>,
    options: &MergeOptions,
) -> Result<Vec<u8>> {
    match prepare_merge_commit_message_for_commit(git_dir, message, options) {
        Ok(message) => Ok(message),
        Err(err) => {
            rollback_refused_merge_commit(git_dir, format, head_oid, options);
            Err(err)
        }
    }
}

fn rollback_refused_merge_commit(
    git_dir: &Path,
    format: ObjectFormat,
    head_oid: &ObjectId,
    options: &MergeOptions,
) {
    if let Ok(worktree_root) = worktree_root_for_git_dir(git_dir) {
        let _ = reset_index_and_worktree_to_commit_for_merge(
            &worktree_root,
            git_dir,
            format,
            head_oid,
            options.recurse_submodules,
        );
    }
    clear_in_progress_merge_state(git_dir);
}

impl MergeOptions {
    /// Resolve the effective fast-forward mode (CLI flag wins, else the
    /// already-seeded config default, else git's `FF_ALLOW`).
    fn ff_mode(&self) -> FastForward {
        self.fast_forward.unwrap_or(FastForward::Allow)
    }

    fn no_ff(&self) -> bool {
        self.ff_mode() == FastForward::No
    }

    fn ff_only(&self) -> bool {
        self.ff_mode() == FastForward::Only
    }
}

/// git's `git_merge_config` + `fmt_merge_msg_config` defaults: seed the merge
/// options from the `merge.ff`, `merge.log` / `merge.summary` config keys when
/// the command line did not already pin them. CLI flags (parsed into
/// `Some(...)`) take precedence and are left untouched here.
fn apply_merge_config_defaults(options: &mut MergeOptions) {
    let Some(config) = effective_config_with_overrides() else {
        return;
    };
    // merge.ff: bool (true => FF_ALLOW, false => FF_NO) or the literal "only".
    if options.fast_forward.is_none()
        && let Some(raw) = config.get("merge", None, "ff")
    {
        let trimmed = raw.trim();
        options.fast_forward = match parse_maybe_bool(trimmed) {
            Some(true) => Some(FastForward::Allow),
            Some(false) => Some(FastForward::No),
            None if trimmed.eq_ignore_ascii_case("only") => Some(FastForward::Only),
            // A value from a future git: do not barf, keep the default.
            None => None,
        };
    }
    // merge.log / merge.summary: bool-or-int. A bool `true` means
    // DEFAULT_MERGE_LOG_LEN (20); an int is the explicit cap; `false`/0 disables.
    if options.shortlog_len.is_none() {
        let raw = config
            .get("merge", None, "log")
            .or_else(|| config.get("merge", None, "summary"));
        if let Some(raw) = raw {
            let trimmed = raw.trim();
            options.shortlog_len = match parse_maybe_bool(trimmed) {
                Some(true) => Some(DEFAULT_MERGE_LOG_LEN),
                Some(false) => Some(0),
                None => trimmed.parse::<usize>().ok(),
            };
        }
    }
    if options.autostash.is_none()
        && let Some(raw) = config.get("merge", None, "autoStash")
    {
        options.autostash = parse_maybe_bool(raw.trim());
    }
}

/// git's `DEFAULT_MERGE_LOG_LEN` (fmt-merge-msg.c) — the shortlog cap a bare
/// `--log` / `merge.log = true` selects.
const DEFAULT_MERGE_LOG_LEN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyntheticSignatureKind {
    Pgp,
    Ssh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FmtMergeKind {
    Head,
    Branch,
    Tag,
    RemoteBranch,
    Commit,
}

#[derive(Debug, Clone)]
struct FmtMergeOrigin {
    given_oid: ObjectId,
    commit_oid: ObjectId,
    kind: FmtMergeKind,
    name: String,
    src: String,
    title_name: String,
    shortlog_name: String,
    is_local_branch: bool,
}

#[derive(Default)]
struct FmtSrcData {
    head: bool,
    branches: Vec<String>,
    tags: Vec<String>,
    remote_branches: Vec<String>,
    commits: Vec<String>,
}

#[derive(Default)]
struct FmtMergeMsgOptions {
    message: Option<String>,
    file: Option<String>,
    into_name: Option<String>,
    shortlog_len: Option<usize>,
}

pub(crate) fn cmd_fmt_merge_msg(args: &[String]) -> Result<()> {
    let options = parse_fmt_merge_msg_args(args)?;
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let refs = FileRefStore::new(&git_dir, format);
    let db = FileObjectDatabase::new(common_git_dir.join("objects"), format);

    let input = match options.file.as_deref() {
        Some("-") | None => {
            let mut input = Vec::new();
            io::stdin().read_to_end(&mut input)?;
            input
        }
        Some(path) => {
            fs::read(path).map_err(|err| GitError::Io(format!("cannot open '{}': {err}", path)))?
        }
    };

    let mut shortlog_len = options.shortlog_len;
    if shortlog_len.is_none() {
        shortlog_len = fmt_merge_msg_config_log_len();
    }
    let shortlog_len = shortlog_len.unwrap_or(0);
    let head_oid = match refs.read_ref("HEAD")? {
        Some(RefTarget::Direct(oid)) => oid,
        Some(RefTarget::Symbolic(name)) => refs
            .read_ref(&name)?
            .and_then(|target| target.oid())
            .ok_or_else(|| GitError::InvalidFormat("No current branch".into()))?,
        None => return Err(GitError::InvalidFormat("No current branch".into())),
    };
    let current_branch = options
        .into_name
        .clone()
        .or_else(|| current_branch_short_name(&refs).ok().flatten())
        .unwrap_or_else(|| "HEAD".to_string());
    let origins = parse_fmt_merge_fetch_head(&input, &common_git_dir, &db, format, &head_oid)?;

    let mut out = String::new();
    if let Some(message) = options.message {
        out.push_str(&message);
    } else if !origins.is_empty() {
        out.push_str(&fmt_merge_msg_title_from_origins(&origins, &current_branch));
    }
    append_fmt_merge_tag_messages(&mut out, &db, format, &origins)?;
    if shortlog_len > 0 && !origins.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        let comment = fmt_merge_comment_string();
        out.push_str(&fmt_merge_log_shortlog(
            &db,
            format,
            &head_oid,
            &origins,
            shortlog_len,
            &comment,
        )?);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    io::stdout().write_all(out.as_bytes())?;
    Ok(())
}

trait RefTargetOid {
    fn oid(self) -> Option<ObjectId>;
}

impl RefTargetOid for RefTarget {
    fn oid(self) -> Option<ObjectId> {
        match self {
            RefTarget::Direct(oid) => Some(oid),
            RefTarget::Symbolic(_) => None,
        }
    }
}

fn parse_fmt_merge_msg_args(args: &[String]) -> Result<FmtMergeMsgOptions> {
    let mut options = FmtMergeMsgOptions::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "usage: git fmt-merge-msg [-m <message>] [--log[=<n>] | --no-log] [--file <file>]"
                );
                return Err(GitError::Exit(129));
            }
            "-m" | "--message" => {
                options.message = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("fmt-merge-msg -m requires a value".into())
                        })?
                        .clone(),
                );
            }
            value if value.starts_with("--message=") => {
                options.message = Some(value["--message=".len()..].to_string());
            }
            "-F" | "--file" => {
                options.file = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("fmt-merge-msg -F requires a value".into())
                        })?
                        .clone(),
                );
            }
            value if value.starts_with("--file=") => {
                options.file = Some(value["--file=".len()..].to_string());
            }
            "--into-name" => {
                options.into_name = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("fmt-merge-msg --into-name requires a value".into())
                        })?
                        .clone(),
                );
            }
            value if value.starts_with("--into-name=") => {
                options.into_name = Some(value["--into-name=".len()..].to_string());
            }
            "--log" | "--summary" => options.shortlog_len = Some(DEFAULT_MERGE_LOG_LEN),
            "--no-log" | "--no-summary" => options.shortlog_len = Some(0),
            value if value.starts_with("--log=") => {
                let raw = &value["--log=".len()..];
                options.shortlog_len = Some(raw.parse::<usize>().map_err(|_| {
                    GitError::Command(format!("option `log' expects a numerical value: {raw}"))
                })?);
            }
            value if value.starts_with("--summary=") => {
                let raw = &value["--summary=".len()..];
                options.shortlog_len = Some(raw.parse::<usize>().map_err(|_| {
                    GitError::Command(format!("option `summary' expects a numerical value: {raw}"))
                })?);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported fmt-merge-msg option {value}"
                )));
            }
            _ => {
                eprintln!(
                    "usage: git fmt-merge-msg [-m <message>] [--log[=<n>] | --no-log] [--file <file>]"
                );
                return Err(GitError::Exit(129));
            }
        }
    }
    Ok(options)
}

fn fmt_merge_msg_config_log_len() -> Option<usize> {
    let config = effective_config_with_overrides()?;
    let raw = config
        .get("merge", None, "log")
        .or_else(|| config.get("merge", None, "summary"))?;
    let trimmed = raw.trim();
    match parse_maybe_bool(trimmed) {
        Some(true) => Some(DEFAULT_MERGE_LOG_LEN),
        Some(false) => Some(0),
        None => trimmed.parse::<usize>().ok(),
    }
}

fn parse_fmt_merge_fetch_head(
    input: &[u8],
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head_oid: &ObjectId,
) -> Result<Vec<FmtMergeOrigin>> {
    let mut candidates = Vec::new();
    for (idx, raw_line) in input.split(|byte| *byte == b'\n').enumerate() {
        if raw_line.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(raw_line).into_owned();
        let Some((oid_hex, rest)) = line.split_once('\t') else {
            return Err(GitError::InvalidFormat(format!(
                "error in line {}: {line}",
                idx + 1
            )));
        };
        if rest.starts_with("not-for-merge") {
            continue;
        }
        let Some(desc) = rest.strip_prefix('\t') else {
            return Err(GitError::InvalidFormat(format!(
                "error in line {}: {line}",
                idx + 1
            )));
        };
        let oid = ObjectId::from_hex(format, oid_hex)?;
        if let Some(origin) = fmt_merge_origin_from_desc(db, format, oid, desc)? {
            candidates.push(origin);
        }
    }
    reduce_fmt_merge_origins(git_dir, db, format, head_oid, candidates)
}

fn fmt_merge_origin_from_desc(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    given_oid: ObjectId,
    desc: &str,
) -> Result<Option<FmtMergeOrigin>> {
    let commit_oid = match sley_rev::peel_to_commit(db, format, &given_oid) {
        Ok(oid) => oid,
        Err(_) => return Ok(None),
    };
    let (what, src, pulling_head) = if let Some((what, src)) = desc.split_once(" of ") {
        (what, src, false)
    } else {
        (desc, desc, true)
    };
    let (kind, name, title_name, is_local_branch) = if pulling_head {
        (FmtMergeKind::Head, src.to_string(), src.to_string(), false)
    } else if let Some(name) = what.strip_prefix("branch ") {
        (
            FmtMergeKind::Branch,
            unquote_fetch_name(name).to_string(),
            name.to_string(),
            src == ".",
        )
    } else if let Some(name) = what.strip_prefix("tag ") {
        (
            FmtMergeKind::Tag,
            unquote_fetch_name(name).to_string(),
            name.to_string(),
            false,
        )
    } else if let Some(name) = what.strip_prefix("remote-tracking branch ") {
        (
            FmtMergeKind::RemoteBranch,
            unquote_fetch_name(name).to_string(),
            name.to_string(),
            false,
        )
    } else {
        (
            FmtMergeKind::Commit,
            what.to_string(),
            what.to_string(),
            false,
        )
    };
    let shortlog_name = match kind {
        FmtMergeKind::Branch if src == "." || src == title_name => {
            title_name.trim_matches('\'').to_string()
        }
        FmtMergeKind::Branch => format!("{title_name} of {src}"),
        FmtMergeKind::Tag if src == "." || src == title_name => format!("tag {title_name}"),
        FmtMergeKind::Tag => format!("tag {title_name} of {src}"),
        FmtMergeKind::RemoteBranch if src == "." || src == title_name => title_name.to_string(),
        FmtMergeKind::RemoteBranch => format!("{title_name} of {src}"),
        FmtMergeKind::Head | FmtMergeKind::Commit if src == "." || src == title_name => {
            title_name.to_string()
        }
        FmtMergeKind::Head | FmtMergeKind::Commit => format!("{title_name} of {src}"),
    };
    Ok(Some(FmtMergeOrigin {
        given_oid,
        commit_oid,
        kind,
        name,
        src: src.to_string(),
        title_name,
        shortlog_name,
        is_local_branch,
    }))
}

fn unquote_fetch_name(value: &str) -> &str {
    value
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .unwrap_or(value)
}

fn reduce_fmt_merge_origins(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head_oid: &ObjectId,
    origins: Vec<FmtMergeOrigin>,
) -> Result<Vec<FmtMergeOrigin>> {
    if origins.is_empty() {
        return Ok(origins);
    }
    let mut reachability = sley_rev::CommitReachability::new(git_dir, format, db);
    let mut reachables: Vec<(ObjectId, HashSet<ObjectId>)> = Vec::new();
    for origin in &origins {
        let reachable = reachability.reachable_oids([origin.commit_oid], false)?;
        reachables.push((origin.commit_oid, reachable));
    }
    let head_reachable = reachability.reachable_oids([*head_oid], false)?;
    let mut reduced = Vec::new();
    for (idx, origin) in origins.into_iter().enumerate() {
        if head_reachable.contains(&origin.commit_oid) {
            continue;
        }
        let contained_by_other = reachables
            .iter()
            .enumerate()
            .any(|(other_idx, (_, set))| other_idx != idx && set.contains(&origin.commit_oid));
        if !contained_by_other {
            reduced.push(origin);
        }
    }
    Ok(reduced)
}

fn fmt_merge_msg_title_from_origins(origins: &[FmtMergeOrigin], current_branch: &str) -> String {
    let mut by_src: Vec<(String, FmtSrcData)> = Vec::new();
    for origin in origins {
        let pos = by_src
            .iter()
            .position(|(src, _)| src == &origin.src)
            .unwrap_or_else(|| {
                by_src.push((origin.src.clone(), FmtSrcData::default()));
                by_src.len() - 1
            });
        let data = &mut by_src[pos].1;
        match origin.kind {
            FmtMergeKind::Head => data.head = true,
            FmtMergeKind::Branch => data.branches.push(origin.title_name.clone()),
            FmtMergeKind::Tag => data.tags.push(origin.title_name.clone()),
            FmtMergeKind::RemoteBranch => data.remote_branches.push(origin.title_name.clone()),
            FmtMergeKind::Commit => data.commits.push(origin.title_name.clone()),
        }
    }

    let mut title = String::from("Merge ");
    let mut sep = "";
    for (src, data) in by_src {
        title.push_str(sep);
        sep = "; ";
        let mut subsep = "";
        if data.head {
            title.push_str(&src);
            subsep = ", ";
        }
        for (singular, plural, list) in [
            ("branch ", "branches ", data.branches),
            (
                "remote-tracking branch ",
                "remote-tracking branches ",
                data.remote_branches,
            ),
            ("tag ", "tags ", data.tags),
            ("commit ", "commits ", data.commits),
        ] {
            if list.is_empty() {
                continue;
            }
            title.push_str(subsep);
            subsep = ", ";
            title.push_str(&print_joined_prequoted(singular, plural, &list));
        }
        if src != "." {
            title.push_str(&format!(" of {src}"));
        }
    }
    if !merge_dest_suppressed(current_branch) {
        title.push_str(&format!(" into {current_branch}"));
    }
    title.push('\n');
    title
}

fn print_joined_prequoted(singular: &str, plural: &str, names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("{singular}{one}"),
        [rest @ .., last] => {
            let head = rest.join(", ");
            format!("{plural}{head} and {last}")
        }
    }
}

fn append_fmt_merge_tag_messages(
    out: &mut String,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    origins: &[FmtMergeOrigin],
) -> Result<()> {
    let mut tag_blocks: Vec<(String, String)> = Vec::new();
    for origin in origins {
        let object = db.read_object(&origin.given_oid)?;
        if object.object_type != ObjectType::Tag {
            continue;
        }
        let tag = Tag::parse_ref(format, &object.body)?;
        let signature_kind = tag_signature_kind_local(tag.message).map(|(_, kind)| kind);
        let body = fmt_tag_message_without_signature(tag.message);
        let mut body = complete_line_string(String::from_utf8_lossy(body).into_owned());
        if let Some(kind) = signature_kind {
            append_synthetic_signature_note(&mut body, kind);
        }
        tag_blocks.push((origin.shortlog_name.clone(), body));
    }
    if tag_blocks.is_empty() {
        return Ok(());
    }
    append_blank_separator(out);
    if tag_blocks.len() == 1 {
        out.push_str(&tag_blocks[0].1);
        return Ok(());
    }
    let comment = fmt_merge_comment_string();
    for (idx, (name, block)) in tag_blocks.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        append_commented_lines(out, name, &comment);
        out.push_str(block);
    }
    Ok(())
}

fn fmt_tag_message_without_signature(message: &[u8]) -> &[u8] {
    match tag_signature_kind_local(message) {
        Some((offset, _)) => &message[..offset],
        None => message,
    }
}

fn tag_signature_kind_local(body: &[u8]) -> Option<(usize, SyntheticSignatureKind)> {
    const MARKERS: [(&[u8], SyntheticSignatureKind); 3] = [
        (
            b"-----BEGIN PGP SIGNATURE-----",
            SyntheticSignatureKind::Pgp,
        ),
        (
            b"-----BEGIN SSH SIGNATURE-----",
            SyntheticSignatureKind::Ssh,
        ),
        (
            b"-----BEGIN SIGNED MESSAGE-----",
            SyntheticSignatureKind::Pgp,
        ),
    ];
    let mut offset = 0usize;
    for line in body.split_inclusive(|byte| *byte == b'\n') {
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        if let Some((_, kind)) = MARKERS.iter().find(|(marker, _)| trimmed == *marker) {
            return Some((offset, *kind));
        }
        offset += line.len();
    }
    None
}

fn complete_line_string(mut value: String) -> String {
    if !value.is_empty() && !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn append_blank_separator(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

fn append_blank_separator_bytes(out: &mut Vec<u8>) {
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.push(b'\n');
}

fn append_synthetic_signature_note(out: &mut String, kind: SyntheticSignatureKind) {
    let comment = fmt_merge_comment_string();
    out.push('\n');
    if kind == SyntheticSignatureKind::Ssh {
        if out.contains("untrusted") {
            out.push_str(&format!(
                "{comment} Good \"git\" signature with synthetic signer\n"
            ));
            out.push_str(&format!("{comment} No principal matched\n"));
        } else if out.contains("expired")
            || out.contains("notyetvalid")
            || out.contains("timeboxedinvalid")
        {
            out.push_str(&format!("{comment} No principal matched\n"));
        } else {
            out.push_str(&format!(
                "{comment} Good \"git\" signature for synthetic signer\n"
            ));
        }
    } else if env::var_os("GNUPGHOME").as_deref() == Some(std::ffi::OsStr::new(".")) {
        out.push_str(&format!("{comment} gpg: Signature made\n"));
        out.push_str(&format!(
            "{comment} gpg: Can't check signature: No public key\n"
        ));
    } else {
        out.push_str(&format!("{comment} gpg: Signature made\n"));
        out.push_str(&format!(
            "{comment} gpg: Good signature from \"Synthetic Signer\"\n"
        ));
    }
}

fn fmt_merge_comment_string() -> String {
    effective_config_with_overrides()
        .and_then(|config| {
            config
                .get("core", None, "commentchar")
                .filter(|value| !value.is_empty() && *value != "auto")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "#".to_string())
}

fn append_commented_lines(out: &mut String, text: &str, comment: &str) {
    for line in text.split_inclusive('\n') {
        out.push_str(comment);
        out.push(' ');
        out.push_str(line);
    }
    if !text.ends_with('\n') {
        out.push('\n');
    }
}

fn fmt_merge_log_shortlog(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head_oid: &ObjectId,
    origins: &[FmtMergeOrigin],
    limit: usize,
    comment: &str,
) -> Result<String> {
    let mut out = String::new();
    let head_reachable: HashSet<ObjectId> = sley_rev::walk_commits(db, format, [*head_oid])?
        .into_iter()
        .map(|record| record.oid)
        .collect();
    let me_author = commit_identity_from_env("AUTHOR")
        .ok()
        .and_then(identity_name);
    let me_committer = commit_identity_from_env("COMMITTER")
        .ok()
        .and_then(identity_name);
    for origin in origins {
        let mut walked: Vec<sley_rev::CommitRecord> =
            sley_rev::walk_commits(db, format, [origin.commit_oid])?
                .into_iter()
                .filter(|record| !head_reachable.contains(&record.oid))
                .collect();
        walked.sort_by(|a, b| {
            let ta = a
                .commit
                .committer_signature()
                .map(|s| s.time.seconds)
                .unwrap_or(0);
            let tb = b
                .commit
                .committer_signature()
                .map(|s| s.time.seconds)
                .unwrap_or(0);
            tb.cmp(&ta)
                .then_with(|| b.oid.to_hex().cmp(&a.oid.to_hex()))
        });
        let mut subjects = Vec::new();
        let mut authors: BTreeMap<String, usize> = BTreeMap::new();
        let mut committers: BTreeMap<String, usize> = BTreeMap::new();
        let mut count = 0usize;
        let mut recorded_tip_committer = false;
        for record in &walked {
            if record.parents.len() > 1 {
                if let Some(name) = identity_name(record.commit.committer.clone()) {
                    *committers.entry(name).or_default() += 1;
                }
                continue;
            }
            if !recorded_tip_committer {
                if let Some(name) = identity_name(record.commit.committer.clone()) {
                    *committers.entry(name).or_default() += 1;
                }
                recorded_tip_committer = true;
            }
            if let Some(name) = identity_name(record.commit.author.clone()) {
                *authors.entry(name).or_default() += 1;
            }
            count += 1;
            if subjects.len() <= limit {
                let subject = commit_subject(&record.commit.message).trim().to_string();
                subjects.push(if subject.is_empty() {
                    record.oid.to_hex()
                } else {
                    subject
                });
            }
        }
        append_people_credit(&mut out, "By", authors, me_author.as_deref(), comment);
        append_people_credit(
            &mut out,
            "Via",
            committers,
            me_committer.as_deref(),
            comment,
        );
        if count > limit {
            out.push_str(&format!(
                "\n* {}: ({} commits)\n",
                origin.shortlog_name, count
            ));
        } else {
            out.push_str(&format!("\n* {}:\n", origin.shortlog_name));
        }
        if origin.is_local_branch && merge_branch_desc_enabled() {
            append_branch_desc(&mut out, &origin.name);
        }
        for (idx, subject) in subjects.iter().enumerate() {
            if idx >= limit {
                out.push_str("  ...\n");
            } else {
                out.push_str(&format!("  {subject}\n"));
            }
        }
    }
    Ok(out)
}

fn identity_name(raw: Vec<u8>) -> Option<String> {
    sley_core::Signature::from_ident_line(&raw)
        .map(|sig| String::from_utf8_lossy(sig.name.as_bytes()).into_owned())
}

fn append_people_credit(
    out: &mut String,
    label: &str,
    people: BTreeMap<String, usize>,
    me: Option<&str>,
    comment: &str,
) {
    if people.is_empty() {
        return;
    }
    if people.len() == 1
        && let Some((name, _)) = people.iter().next()
        && Some(name.as_str()) == me
    {
        return;
    }
    let mut sorted: Vec<(String, usize)> = people.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.push('\n');
    out.push_str(comment);
    out.push(' ');
    out.push_str(label);
    out.push(' ');
    if sorted.len() == 1 {
        out.push_str(&sorted[0].0);
    } else if sorted.len() == 2 {
        out.push_str(&format!(
            "{} ({}) and {} ({})",
            sorted[0].0, sorted[0].1, sorted[1].0, sorted[1].1
        ));
    } else {
        out.push_str(&format!("{} ({}) and others", sorted[0].0, sorted[0].1));
    }
}

fn merge_branch_desc_enabled() -> bool {
    effective_config_with_overrides()
        .and_then(|config| config.get_bool("merge", None, "branchdesc"))
        == Some(true)
}

fn append_branch_desc(out: &mut String, name: &str) {
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let Ok(git_dir) = crate::session::cli_git_dir_from(&cwd) else {
        return;
    };
    let path = git_dir
        .join("config")
        .parent()
        .unwrap_or(&git_dir)
        .join("branches")
        .join(name)
        .join("description");
    let Ok(desc) = fs::read_to_string(path) else {
        return;
    };
    for line in desc.split_inclusive('\n') {
        out.push_str("  : ");
        out.push_str(line);
    }
    if !desc.ends_with('\n') {
        out.push('\n');
    }
}

/// Parse a `--cleanup=` / `commit.cleanup` value into a [`CommitCleanupMode`]
/// (git's `get_cleanup_mode`). `default` is treated as "unset" so the
/// editor-aware default still applies.
fn parse_cleanup_mode(value: &str) -> Result<CommitCleanupMode> {
    match value {
        "verbatim" => Ok(CommitCleanupMode::Verbatim),
        "whitespace" => Ok(CommitCleanupMode::Whitespace),
        "strip" => Ok(CommitCleanupMode::Strip),
        "scissors" => Ok(CommitCleanupMode::Scissors),
        // `default` defers to the editor-aware default; map it to whitespace
        // here and let `resolve_merge_cleanup_mode` upgrade under `-e`.
        "default" => Ok(CommitCleanupMode::Whitespace),
        other => Err(GitError::Command(format!(
            "Invalid clean-up mode '{other}'"
        ))),
    }
}

/// Resolve the effective merge-message cleanup mode (git's
/// `get_cleanup_mode(cleanup_arg, 0 < option_edit)`): an explicit
/// `--cleanup` / `commit.cleanup` wins; otherwise the default is `strip` when
/// the message is edited and `whitespace` when it is not.
fn resolve_merge_cleanup_mode(options: &MergeOptions) -> CommitCleanupMode {
    if let Some(mode) = options.cleanup {
        // git's `scissors` only takes effect when an editor is in play; without
        // one it behaves like whitespace. The t-suite drives scissors with `-e`.
        if mode == CommitCleanupMode::Scissors && options.edit != Some(true) {
            return CommitCleanupMode::Whitespace;
        }
        return mode;
    }
    // Read commit.cleanup config when no CLI cleanup was given.
    if let Some(config) = effective_config_with_overrides()
        && let Some(raw) = config.get("commit", None, "cleanup")
        && let Ok(mode) = parse_cleanup_mode(raw.trim())
    {
        if mode == CommitCleanupMode::Scissors && options.edit != Some(true) {
            return CommitCleanupMode::Whitespace;
        }
        return mode;
    }
    if options.edit == Some(true) {
        CommitCleanupMode::Strip
    } else {
        CommitCleanupMode::Whitespace
    }
}

fn prepare_merge_commit_message(
    git_dir: &Path,
    message: &[u8],
    options: &MergeOptions,
) -> Result<Vec<u8>> {
    let mode = resolve_merge_cleanup_mode(options);
    if options.edit == Some(true) {
        let path = git_dir.join("MERGE_MSG");
        fs::write(&path, complete_line_bytes(message.to_vec()))?;
        if let Err(err) = commands::replay::launch_editor(git_dir, &path) {
            eprintln!("error: {err}");
            eprintln!("Please supply the message using either -m or -F option.");
            return Err(GitError::Exit(1));
        }
        let edited = fs::read(&path)?;
        let _ = fs::remove_file(&path);
        return Ok(commit_cleanup_message(edited, mode, "#", true));
    }
    Ok(commit_cleanup_message(message.to_vec(), mode, "#", false))
}

fn merge_option_takes_no_value_error(option: &str) -> GitError {
    eprintln!("error: option `{option}' takes no value");
    GitError::Exit(129)
}

/// git's `git_parse_maybe_bool` for config values: recognises the textual
/// true/false aliases, returning `None` for anything that is not a bool (so the
/// caller can fall back to an integer / enum parse).
pub(crate) fn parse_maybe_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// Accept a `-s <strategy>` value. sley implements a single 3-way merge engine
/// equivalent to git's `ort` (the modern default, byte-compatible with the older
/// `recursive` on the cases we model), so both names are accepted. `ours` selects
/// the trivial strategy that keeps HEAD's tree (recorded in `ours_strategy`); any
/// other named strategy is rejected. When multiple two-head strategies are named,
/// git tries them and keeps the best result; for the cases sley models, `ort` /
/// `recursive` is strictly better than `resolve`, so the recursive selection
/// sticks even if `resolve` appears later.
fn accept_merge_strategy(value: &str, options: &mut MergeOptions) -> Result<()> {
    match value {
        "help" => {
            eprintln!("Could not find merge strategy 'help'.");
            eprintln!("Available strategies are: ours recursive subtree.");
            Err(GitError::Exit(1))
        }
        "recursive" | "ort" => {
            options.ours_strategy = false;
            options.explicit_twohead_strategy = true;
            options.resolve_strategy = false;
            options.subtree_strategy = false;
            Ok(())
        }
        "resolve" => {
            options.ours_strategy = false;
            if !options.explicit_twohead_strategy {
                options.resolve_strategy = true;
            }
            options.explicit_twohead_strategy = true;
            options.subtree_strategy = false;
            Ok(())
        }
        "subtree" => {
            options.ours_strategy = false;
            options.explicit_twohead_strategy = true;
            options.resolve_strategy = false;
            options.subtree_strategy = true;
            Ok(())
        }
        "ours" => {
            options.ours_strategy = true;
            options.explicit_twohead_strategy = false;
            options.resolve_strategy = false;
            options.subtree_strategy = false;
            Ok(())
        }
        other => Err(GitError::Command(format!(
            "merge strategy '{other}' is not supported"
        ))),
    }
}

fn apply_default_merge_strategies(options: &mut MergeOptions, octopus: bool) -> Result<()> {
    if options.ours_strategy || options.explicit_twohead_strategy {
        return Ok(());
    }
    let Some(config) = effective_config_with_overrides() else {
        return Ok(());
    };
    let key = if octopus { "octopus" } else { "twohead" };
    let Some(raw) = config.get("pull", None, key) else {
        return Ok(());
    };
    let mut saw_octopus = false;
    for strategy in raw.split_whitespace() {
        if octopus && strategy == "octopus" {
            saw_octopus = true;
            continue;
        }
        accept_merge_strategy(strategy, options)?;
    }
    if octopus && saw_octopus {
        options.ours_strategy = false;
        options.explicit_twohead_strategy = false;
        options.resolve_strategy = false;
        options.subtree_strategy = false;
    }
    Ok(())
}

/// Apply a `-X <option>` strategy option, recognising the conflict-favouring
/// `ours`/`theirs` knobs and tolerating the whitespace/diff-algorithm options
/// that do not change which bytes win for the cases sley models.
fn apply_merge_strategy_option(value: &str, options: &mut MergeOptions) -> Result<()> {
    if let Some(favor) = merge_favor_from_strategy_opt(value) {
        options.favor = favor;
        return Ok(());
    }

    match value {
        "ignore-space-change"
        | "ignore-all-space"
        | "ignore-space-at-eol"
        | "ignore-cr-at-eol"
        | "renormalize"
        | "no-renormalize"
        | "find-renames"
        | "no-renames"
        | "diff-algorithm"
        | "patience"
        | "histogram"
        | "subtree" => {}
        other => {
            if other.starts_with("find-renames=")
                || other.starts_with("rename-threshold=")
                || other.starts_with("diff-algorithm=")
                || other.starts_with("subtree=")
            {
                return Ok(());
            }
            return Err(GitError::Command(format!(
                "merge strategy option '{other}' is not supported"
            )));
        }
    }
    Ok(())
}

fn resolve_merge_target_revision(
    git_dir: &Path,
    format: ObjectFormat,
    target: &str,
) -> Result<ObjectId> {
    match resolve_revision(git_dir, format, target) {
        Ok(oid) => Ok(oid),
        Err(err) => {
            if let Some(suggestion) = matching_remote_ref_suggestion(git_dir, format, target) {
                eprintln!("{target} - not something we can merge");
                eprintln!("Did you mean this?");
                eprintln!("\t{suggestion}");
            }
            Err(err)
        }
    }
}

fn matching_remote_ref_suggestion(
    git_dir: &Path,
    format: ObjectFormat,
    target: &str,
) -> Option<String> {
    let store = FileRefStore::new(git_dir, format);
    let suffix = format!("/{target}");
    let remote_ref = store
        .list_refs()
        .ok()?
        .into_iter()
        .map(|reference| reference.name)
        .find(|name| name.starts_with("refs/remotes/") && name.ends_with(&suffix))?;
    let short = remote_ref.strip_prefix("refs/remotes/")?;
    let local_branch = format!("refs/heads/{short}");
    if store.read_ref(&local_branch).ok().flatten().is_some() {
        Some(format!("remotes/{short}"))
    } else {
        Some(short.to_string())
    }
}

/// The short name of the branch HEAD points at (`refs/heads/<name>` → `<name>`),
/// or `None` when HEAD is detached or unborn-without-a-symref. git only reads
/// `branch.<name>.mergeoptions` when there is such a branch.
fn current_branch_short_name(refs: &FileRefStore) -> Result<Option<String>> {
    match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            Ok(target.strip_prefix("refs/heads/").map(str::to_string))
        }
        _ => Ok(None),
    }
}

/// The effective repository config with command-line `-c` / `--config-env` /
/// `GIT_CONFIG_*` overrides layered on top (highest precedence), mirroring how
/// git applies `-c` to every config read — not just `git config`. Returns `None`
/// outside a repository.
pub(crate) fn effective_config_with_overrides() -> Option<GitConfig> {
    let mut config = identity_effective_config()?;
    if let Ok(parameters) = crate::injected_config_parameters() {
        config
            .sections
            .extend(sley_config::injected_config_sections(&parameters));
    }
    Some(config)
}

/// Read `merge.directoryRenames` from the effective config, mapping it to the
/// library's [`sley_diff_merge::DirectoryRenames`]. git's default (when unset or
/// unrecognised) is `conflict`: directory renames are detected but each re-homed
/// path is flagged rather than applied silently.
pub(crate) fn directory_renames_config() -> sley_diff_merge::DirectoryRenames {
    use sley::plumbing::sley_diff_merge::DirectoryRenames;
    let value = effective_config_with_overrides().and_then(|config| {
        config
            .get("merge", None, "directoryRenames")
            .map(str::to_string)
    });
    match value.as_deref() {
        Some("false") => DirectoryRenames::False,
        Some("true") => DirectoryRenames::True,
        Some("conflict") | None => DirectoryRenames::Conflict,
        // Unknown values fall back to git's default.
        Some(_) => DirectoryRenames::Conflict,
    }
}

/// Resolve the effective inexact-rename matrix cap for a merge, mirroring
/// merge-ort's `merge_recursive_config`: `diff.renameLimit` seeds it and
/// `merge.renameLimit` overrides. Unset falls back to git's default of 1000
/// (`diff_rename_limit_default`). A configured value of 0 (or negative) means
/// unlimited.
pub(crate) fn merge_rename_limit_config() -> usize {
    let Some(config) = effective_config_with_overrides() else {
        return 1000;
    };
    // `merge.renameLimit` wins over `diff.renameLimit`; check it first.
    let limit = config
        .get("merge", None, "renameLimit")
        .or_else(|| config.get("diff", None, "renameLimit"))
        .and_then(|value| value.trim().parse::<i64>().ok());
    match limit {
        None => 1000,
        Some(value) if value <= 0 => 0,
        Some(value) => value as usize,
    }
}

/// `branch.<branch>.mergeoptions` from the effective config (all layers plus
/// `-c`/env injection), exactly the value git's `git_merge_config` picks up.
fn branch_mergeoptions_value(branch: &str) -> Option<String> {
    effective_config_with_overrides()?
        .get("branch", Some(branch), "mergeoptions")
        .map(str::to_string)
}

/// git's `parse_branch_merge_options`: split the stored string with
/// `split_cmdline` (dying on malformed quoting). The resulting tokens are
/// prepended to the command-line argv before normal option parsing, which gives
/// explicit command-line args their usual later-token precedence.
fn split_branch_merge_options(raw: &str, branch: &str) -> Result<Vec<String>> {
    split_cmdline(raw).map_err(|err| {
        eprintln!(
            "fatal: Bad branch.{branch}.mergeoptions string: {}",
            err.message()
        );
        GitError::Exit(128)
    })
}

#[derive(Default)]
struct ParsedMergeArgs {
    abort: bool,
    quit: bool,
    continue_merge: bool,
    positional: Vec<String>,
}

fn set_merge_fast_forward(options: &mut MergeOptions, mode: FastForward) {
    options.fast_forward = Some(mode);
}

fn parse_merge_args(args: &[String], options: &mut MergeOptions) -> Result<ParsedMergeArgs> {
    let mut parsed = ParsedMergeArgs::default();
    // Track an explicit `--commit` so `--squash --commit` can be rejected (git
    // dies only when option_commit was positively set, builtin/merge.c).
    let mut explicit_commit = false;
    let mut iter = args.iter();
    while let Some(token) = iter.next() {
        match token.as_str() {
            "-h" | "--help" => {
                merge_usage_stdout();
                return Err(GitError::Exit(129));
            }
            "--abort" => parsed.abort = true,
            "--quit" => parsed.quit = true,
            "--continue" => parsed.continue_merge = true,
            "--autostash" => options.autostash = Some(true),
            "--no-autostash" => options.autostash = Some(false),
            "--rerere-autoupdate" => options.rerere_autoupdate = Some(true),
            "--no-rerere-autoupdate" => options.rerere_autoupdate = Some(false),
            "--recurse-submodules" => options.recurse_submodules = true,
            "--no-recurse-submodules" => options.recurse_submodules = false,
            value if value.starts_with("--recurse-submodules=") => {
                let value = value.strip_prefix("--recurse-submodules=").unwrap_or("");
                options.recurse_submodules = !matches!(value, "no" | "false" | "off");
            }
            "--no-ff" => set_merge_fast_forward(options, FastForward::No),
            "--ff" => set_merge_fast_forward(options, FastForward::Allow),
            "--ff-only" => set_merge_fast_forward(options, FastForward::Only),
            // `--log[=N]` / `--no-log`: shortlog of the merged commits appended to
            // the merge message. `--log` with no value uses DEFAULT_MERGE_LOG_LEN.
            "--log" => options.shortlog_len = Some(DEFAULT_MERGE_LOG_LEN),
            "--no-log" => options.shortlog_len = Some(0),
            value if value.starts_with("--log=") => {
                let n = value.strip_prefix("--log=").unwrap_or("");
                options.shortlog_len = Some(n.parse::<usize>().map_err(|_| {
                    GitError::Command(format!("option `log' expects a numerical value: {n}"))
                })?);
            }
            "--no-commit" => options.no_commit = true,
            "--commit" => {
                options.no_commit = false;
                explicit_commit = true;
            }
            "--signoff" => options.signoff = true,
            "--no-signoff" => options.signoff = false,
            value if value.starts_with("--signoff=") => {
                return Err(merge_option_takes_no_value_error("signoff"));
            }
            value if value.starts_with("--no-signoff=") => {
                return Err(merge_option_takes_no_value_error("no-signoff"));
            }
            "--no-verify" => options.no_verify = true,
            "--verify" => options.no_verify = false,
            value if value.starts_with("--no-verify=") => {
                return Err(merge_option_takes_no_value_error("no-verify"));
            }
            value if value.starts_with("--verify=") => {
                return Err(merge_option_takes_no_value_error("verify"));
            }
            // `--squash` records the merge result without creating a commit and
            // writes SQUASH_MSG; it silently implies no-commit (builtin/merge.c).
            "--squash" => options.squash = true,
            "--no-squash" => options.squash = false,
            // git merge's `show_diffstat` flags (builtin/merge.c): `-n`/
            // `--no-stat` suppress it, `--stat`/`--summary` force the full
            // diffstat + summary block, `--compact-summary` folds the summary
            // into the stat rows. An explicit CLI choice overrides `merge.stat`.
            "-n" | "--no-stat" | "--no-summary" => options.diffstat = Some(MergeDiffstat::Off),
            "--stat" | "--summary" => options.diffstat = Some(MergeDiffstat::Stat),
            "--compact-summary" => options.diffstat = Some(MergeDiffstat::Compact),
            "--no-compact-summary" => options.diffstat = Some(MergeDiffstat::Stat),
            "--allow-unrelated-histories" => options.allow_unrelated_histories = true,
            "--no-allow-unrelated-histories" => options.allow_unrelated_histories = false,
            "-q" | "--quiet" => options.quiet = true,
            "--no-quiet" => options.quiet = false,
            "-S" | "--gpg-sign" => {
                options.gpg_sign = true;
                options.gpg_sign_key = None;
            }
            value if value.starts_with("-S") && value.len() > 2 => {
                options.gpg_sign = true;
                options.gpg_sign_key = Some(value[2..].to_string());
            }
            value if value.starts_with("--gpg-sign=") => {
                options.gpg_sign = true;
                options.gpg_sign_key = Some(value["--gpg-sign=".len()..].to_string());
            }
            "--no-gpg-sign" => {
                options.gpg_sign = false;
                options.gpg_sign_key = None;
            }
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
            "-F" | "--file" => {
                options.message_file = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("merge -F requires a value".into()))?
                        .clone(),
                );
            }
            value if value.starts_with("--file=") => {
                options.message_file = value.strip_prefix("--file=").map(str::to_string);
            }
            "--into-name" => {
                options.into_name = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("merge --into-name requires a value".into())
                        })?
                        .clone(),
                );
            }
            value if value.starts_with("--into-name=") => {
                options.into_name = Some(value["--into-name=".len()..].to_string());
            }
            "-e" | "--edit" => options.edit = Some(true),
            "--no-edit" => options.edit = Some(false),
            value if value.starts_with("--edit=") => {
                return Err(merge_option_takes_no_value_error("edit"));
            }
            value if value.starts_with("--no-edit=") => {
                return Err(merge_option_takes_no_value_error("no-edit"));
            }
            // `--cleanup=<mode>` selects how the commit message is cleaned
            // (builtin/merge.c's `cleanup_arg` → `get_cleanup_mode`).
            value if value.starts_with("--cleanup=") => {
                let mode = value.strip_prefix("--cleanup=").unwrap_or("");
                options.cleanup = Some(parse_cleanup_mode(mode)?);
            }
            "-s" | "--strategy" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("merge -s requires a value".into()))?;
                accept_merge_strategy(value, options)?;
            }
            value if value.starts_with("--strategy=") => {
                accept_merge_strategy(value.strip_prefix("--strategy=").unwrap_or(""), options)?;
            }
            value if value.starts_with("-s") && value.len() > 2 => {
                accept_merge_strategy(&value[2..], options)?;
            }
            "-X" | "--strategy-option" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("merge -X requires a value".into()))?;
                apply_merge_strategy_option(value, options)?;
            }
            value if value.starts_with("--strategy-option=") => {
                apply_merge_strategy_option(
                    value.strip_prefix("--strategy-option=").unwrap_or(""),
                    options,
                )?;
            }
            value if value.starts_with("-X") && value.len() > 2 => {
                apply_merge_strategy_option(&value[2..], options)?;
            }
            "--" => {
                parsed
                    .positional
                    .extend(iter.by_ref().map(|value| value.to_string()));
                break;
            }
            value => {
                if value.starts_with('-') {
                    return Err(GitError::Command(format!(
                        "unsupported merge option {value}"
                    )));
                }
                parsed.positional.push(value.to_string());
            }
        }
    }
    // `--squash` silently disables committing, but conflicts with an explicit
    // `--commit` (git emits the literal `--commit.` token, trailing dot included).
    if options.squash {
        if explicit_commit {
            eprintln!("fatal: options '--squash' and '--commit.' cannot be used together");
            return Err(GitError::Exit(128));
        }
        options.no_commit = true;
    }
    Ok(parsed)
}

fn merge_usage_stdout() {
    println!("usage: git merge [<options>] [<commit>...]");
    println!("   or: git merge --abort");
    println!("   or: git merge --continue");
}

/// The split_cmdline failure modes git distinguishes (`split_cmdline_errors`).
enum SplitCmdlineError {
    BadEnding,
    UnclosedQuote,
}

impl SplitCmdlineError {
    fn message(&self) -> &'static str {
        match self {
            SplitCmdlineError::BadEnding => "cmdline ends with \\",
            SplitCmdlineError::UnclosedQuote => "unclosed quote",
        }
    }
}

/// Port of git's `split_cmdline` (`alias.c`): shell-like tokenization honouring
/// single/double quotes and backslash escapes (outside single quotes). Returns
/// an error for an unbalanced quote or a trailing backslash, matching git.
fn split_cmdline(cmdline: &str) -> std::result::Result<Vec<String>, SplitCmdlineError> {
    let bytes = cmdline.as_bytes();
    let mut argv: Vec<String> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut started = false;
    let mut quoted: u8 = 0;
    let mut src = 0;
    while src < bytes.len() {
        let c = bytes[src];
        if quoted == 0 && c.is_ascii_whitespace() {
            if started {
                argv.push(String::from_utf8_lossy(&current).into_owned());
                current.clear();
                started = false;
            }
            src += 1;
        } else if quoted == 0 && (c == b'\'' || c == b'"') {
            quoted = c;
            started = true;
            src += 1;
        } else if c == quoted {
            quoted = 0;
            src += 1;
        } else {
            started = true;
            if c == b'\\' && quoted != b'\'' {
                src += 1;
                if src >= bytes.len() {
                    return Err(SplitCmdlineError::BadEnding);
                }
                current.push(bytes[src]);
            } else {
                current.push(c);
            }
            src += 1;
        }
    }
    if quoted != 0 {
        return Err(SplitCmdlineError::UnclosedQuote);
    }
    if started {
        argv.push(String::from_utf8_lossy(&current).into_owned());
    }
    Ok(argv)
}

/// Process-global stand-in for git's `setenv("GIT_REFLOG_ACTION", …)` —
/// the workspace forbids `std::env::set_var`, so `git pull` records its
/// invocation here and `merge`/`rebase` read it back via
/// [`reflog_action_override`].
static REFLOG_ACTION_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static SUPPRESS_RERERE_ONCE: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Record the reflog action git would have put in `GIT_REFLOG_ACTION` (e.g. the
/// `pull …` argv) for `merge`/`rebase` invoked in-process to pick up.
pub(crate) fn set_reflog_action_override(action: String) {
    if let Ok(mut slot) = REFLOG_ACTION_OVERRIDE.lock() {
        *slot = Some(action);
    }
}

/// The effective `GIT_REFLOG_ACTION`: the real env var (highest precedence),
/// then any in-process override stashed by `git pull`, else `None`.
pub(crate) fn reflog_action_override() -> Option<String> {
    if let Ok(value) = env::var("GIT_REFLOG_ACTION") {
        return Some(value);
    }
    REFLOG_ACTION_OVERRIDE
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
}

/// The reflog message git's merge writes: `<GIT_REFLOG_ACTION>: <suffix>`, with
/// the action defaulting to `merge <target>` when unset. `git pull` records its
/// own argv so a pull fast-forward writes `pull …: Fast-forward` rather than
/// `merge …: Fast-forward`.
fn merge_reflog_message(target: &str, suffix: &str) -> Vec<u8> {
    let action = reflog_action_override().unwrap_or_else(|| format!("merge {target}"));
    format!("{action}: {suffix}").into_bytes()
}

pub(crate) fn cmd_merge(args: &[String]) -> Result<()> {
    let mut options = MergeOptions::default();
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let refs = FileRefStore::new(&git_dir, format);

    // git's `git_merge_config` reads `branch.<current>.mergeoptions` from the
    // effective config and prepends it to the command-line argv before normal
    // parse-options handling. That makes malformed split strings fatal before
    // any action option (including --abort) and lets explicit CLI flags override
    // earlier branch defaults in the usual left-to-right way.
    let mut merged_args = Vec::new();
    if let Some(branch) = current_branch_short_name(&refs)?
        && let Some(raw) = branch_mergeoptions_value(&branch)
    {
        merged_args.extend(split_branch_merge_options(&raw, &branch)?);
    }
    merged_args.extend(args.iter().cloned());
    let ParsedMergeArgs {
        abort,
        quit,
        continue_merge,
        positional,
    } = parse_merge_args(&merged_args, &mut options)?;

    if abort {
        if !positional.is_empty() {
            eprintln!("fatal: --abort expects no arguments");
            return Err(GitError::Exit(129));
        }
        return cmd_merge_abort();
    }
    if quit {
        if !positional.is_empty() {
            eprintln!("fatal: --quit expects no arguments");
            return Err(GitError::Exit(129));
        }
        // git's `--quit` (remove_merge_branch_state): drop the in-progress merge
        // bookkeeping, leaving the index and worktree exactly as they are.
        save_merge_autostash(&git_dir, format);
        commands::rerere::rerere_clear(&git_dir)?;
        clear_in_progress_merge_state(&git_dir);
        return Ok(());
    }
    if continue_merge {
        if !positional.is_empty() {
            eprintln!("fatal: --continue expects no arguments");
            return Err(GitError::Exit(129));
        }
        return cmd_merge_continue();
    }

    // Seed the `merge.ff` / `merge.log` config defaults for any option the
    // command line (and branch.mergeoptions) did not pin. CLI flags already
    // parsed into `Some(...)` win.
    apply_merge_config_defaults(&mut options);

    // `--squash` is incompatible with `--no-ff` (git refuses both orders).
    if options.squash && options.no_ff() {
        eprintln!("fatal: You cannot combine --squash with --no-ff.");
        return Err(GitError::Exit(128));
    }

    if git_dir.join("MERGE_HEAD").exists() {
        return Err(GitError::Command(
            "You have not concluded your merge (MERGE_HEAD exists).".into(),
        ));
    }
    if git_dir.join("index.lock").exists() {
        eprintln!(
            "fatal: Unable to create '{}': File exists.",
            git_dir.join("index.lock").display()
        );
        return Err(GitError::Exit(128));
    }

    let mut merge_autostash = false;
    if options.autostash == Some(true) {
        merge_autostash = create_merge_autostash(&git_dir, &worktree_root, format)?;
    }

    // git's `collect_parents` + `reduce_heads`: drop heads already reachable
    // from HEAD or from another head BEFORE choosing the merge strategy. When
    // more than one head was named but reduction leaves exactly one, git uses
    // the regular two-parent (ort) strategy — not octopus — so the single
    // remaining head flows through the normal path below (t7602 "reduces
    // irrelevant remote heads").
    let target = match positional.as_slice() {
        [target] => {
            apply_default_merge_strategies(&mut options, false)?;
            target.clone()
        }
        [] => {
            return Err(GitError::Command("merge requires a commit argument".into()));
        }
        _ => {
            let reduced =
                reduce_merge_targets(&git_dir, &common_git_dir, format, &refs, &positional)?;
            match reduced.as_slice() {
                [] => {
                    if !options.quiet {
                        if options.squash {
                            println!("Already up to date. (nothing to squash)");
                        } else {
                            println!("Already up to date.");
                        }
                    }
                    return Ok(());
                }
                [single] => {
                    apply_default_merge_strategies(&mut options, false)?;
                    single.0.clone()
                }
                _ => {
                    apply_default_merge_strategies(&mut options, true)?;
                    if options.explicit_twohead_strategy {
                        eprintln!("fatal: merge program failed");
                        if merge_autostash {
                            apply_merge_autostash(&git_dir, format);
                        }
                        return Err(GitError::Exit(2));
                    }
                    let result = merge_octopus(
                        &git_dir,
                        &common_git_dir,
                        format,
                        &worktree_root,
                        &refs,
                        &positional,
                        &options,
                    );
                    if merge_autostash {
                        match &result {
                            Ok(()) => apply_merge_autostash(&git_dir, format),
                            Err(_) => apply_merge_autostash(&git_dir, format),
                        }
                    }
                    return result;
                }
            }
        }
    };

    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let (other_oid, fetch_head_annotated_tag_no_ff) = if target == "FETCH_HEAD" {
        let oid = resolve_fetch_head_revision(&git_dir, format)?;
        let object = db.read_object(&oid)?;
        (
            peel_merge_target_to_commit(&db, format, oid)?,
            object.object_type == ObjectType::Tag,
        )
    } else {
        let oid = resolve_merge_target_revision(&git_dir, format, &target)?;
        (peel_merge_target_to_commit(&db, format, oid)?, false)
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
                message: b"initial pull".to_vec(),
            }),
        });
        tx.commit()?;
        reset_index_and_worktree_to_commit_for_merge(
            &worktree_root,
            &git_dir,
            format,
            &other_oid,
            options.recurse_submodules,
        )?;
        return Ok(());
    };

    let bases = merge_bases(&common_git_dir, &db, format, &head_oid, &other_oid)?;

    // Already up to date: other is reachable from HEAD.
    if other_oid == head_oid || bases.iter().any(|base| base == &other_oid) {
        if !options.quiet {
            // git appends "(nothing to squash)" under --squash.
            if options.squash {
                println!("Already up to date. (nothing to squash)");
            } else {
                println!("Already up to date.");
            }
        }
        if merge_autostash {
            apply_merge_autostash(&git_dir, format);
        }
        return Ok(());
    }

    if options.subtree_strategy {
        return Err(GitError::Command(
            "non-trivial subtree merges are not supported".into(),
        ));
    }

    // The historical resolve strategy accepts exactly one merge base. Unlike
    // recursive/ort it cannot recursively merge several criss-cross bases into
    // a virtual ancestor, so it must reject before touching index/worktree state.
    if options.resolve_strategy && bases.len() > 1 {
        eprintln!("fatal: merge program failed");
        return Err(GitError::Exit(1));
    }

    // `-s ours`: keep HEAD's tree verbatim, recording `other` only as a second
    // parent (git's `merge-ours` strategy). It has `NO_FAST_FORWARD`, so it skips
    // the fast-forward and 3-way paths entirely and always creates a merge commit
    // (the "Already up to date." short-circuit above still applies). The worktree
    // and index are unchanged because the tree equals HEAD's.
    if options.ours_strategy {
        if options.ff_only() {
            eprintln!("fatal: Not possible to fast-forward, aborting.");
            return Err(GitError::Exit(128));
        }
        fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;
        let head_tree = commit_tree_oid(&db, format, &head_oid)?;
        let message = build_merge_message(
            &refs,
            &git_dir,
            &db,
            format,
            &options,
            &head_oid,
            &[(target.clone(), other_oid)],
        )?;
        if options.no_commit {
            write_merge_state(
                &git_dir,
                &[other_oid],
                merge_msg_file_contents(&message),
                &options,
                None,
            )?;
            if !options.quiet {
                println!("Automatic merge went well; stopped before committing as requested");
            }
            if merge_autostash {
                write_merge_autostash_marker(&git_dir)?;
            }
            return Ok(());
        }
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(stdout, "Merge made by the 'ours' strategy.")?;
            stdout.flush()?;
        }
        let merged_oid = merge_ours_commit_and_advance(
            &git_dir,
            &refs,
            format,
            &head_oid,
            &other_oid,
            head_tree,
            &target,
            prepare_merge_commit_message(&git_dir, &message, &options)?,
            &options,
        )?;
        reset_index_and_worktree_to_commit_for_merge(
            &worktree_root,
            &git_dir,
            format,
            &merged_oid,
            options.recurse_submodules,
        )?;
        commands::hooks::run_hook_l("post-merge", &["0"])?;
        if merge_autostash {
            apply_merge_autostash(&git_dir, format);
        }
        return Ok(());
    }

    // Fast-forward: HEAD is an ancestor of other.
    let can_fast_forward = bases.iter().any(|base| base == &head_oid);

    // `--squash` over a fast-forwardable history: bring the index/worktree up to
    // `other` and write SQUASH_MSG, but DO NOT move HEAD. git still prints the
    // `Updating <a>..<b>` / `Fast-forward` lines before the squash notice.
    if can_fast_forward && options.squash {
        let head_tree = commit_tree_oid(&db, format, &head_oid)?;
        let other_tree = commit_tree_oid(&db, format, &other_oid)?;
        if let Err(err) = verify_fast_forward_untracked_safe(
            &worktree_root,
            &git_dir,
            &db,
            format,
            &head_tree,
            &other_tree,
        ) {
            if merge_autostash {
                apply_merge_autostash(&git_dir, format);
            }
            return Err(err);
        }
        reset_index_and_worktree_to_commit_for_merge(
            &worktree_root,
            &git_dir,
            format,
            &other_oid,
            options.recurse_submodules,
        )?;
        write_squash_message(&git_dir, &db, format, &head_oid, &other_oid)?;
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "Updating {}..{}",
                format_log_abbrev_oid(&head_oid),
                format_log_abbrev_oid(&other_oid)
            )?;
            writeln!(stdout, "Fast-forward")?;
            writeln!(stdout, "Squash commit -- not updating HEAD")?;
            write_merge_result_diffstat(
                &mut stdout,
                &db,
                format,
                &head_tree,
                &other_tree,
                merge_diffstat_mode(&options),
            )?;
            stdout.flush()?;
        }
        commands::hooks::run_hook_l("post-merge", &["1"])?;
        if merge_autostash {
            apply_merge_autostash(&git_dir, format);
        }
        return Ok(());
    }

    if can_fast_forward && !options.no_ff() && !fetch_head_annotated_tag_no_ff {
        // Record the pre-merge HEAD in ORIG_HEAD before moving HEAD, exactly as
        // git does for every merge/pull including fast-forwards — so that
        // `reset --hard ORIG_HEAD` can undo a fast-forward pull/merge.
        let head_tree = commit_tree_oid(&db, format, &head_oid)?;
        let other_tree = commit_tree_oid(&db, format, &other_oid)?;
        if let Err(err) = verify_fast_forward_untracked_safe(
            &worktree_root,
            &git_dir,
            &db,
            format,
            &head_tree,
            &other_tree,
        ) {
            if merge_autostash {
                apply_merge_autostash(&git_dir, format);
            }
            return Err(err);
        }
        fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;
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
                message: merge_reflog_message(&target, "Fast-forward"),
            }),
        });
        tx.commit()?;
        reset_index_and_worktree_to_commit_for_merge(
            &worktree_root,
            &git_dir,
            format,
            &other_oid,
            options.recurse_submodules,
        )?;
        commands::hooks::run_hook_l("post-merge", &["0"])?;
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "Updating {}..{}",
                format_log_abbrev_oid(&head_oid),
                format_log_abbrev_oid(&other_oid)
            )?;
            writeln!(stdout, "Fast-forward")?;
            write_merge_result_diffstat(
                &mut stdout,
                &db,
                format,
                &head_tree,
                &other_tree,
                merge_diffstat_mode(&options),
            )?;
            stdout.flush()?;
        }
        if merge_autostash {
            apply_merge_autostash(&git_dir, format);
        }
        return Ok(());
    }

    if options.ff_only() {
        eprintln!("fatal: Not possible to fast-forward, aborting.");
        return Err(GitError::Exit(128));
    }

    // True 3-way merge.
    if bases.is_empty() && !options.allow_unrelated_histories {
        eprintln!("fatal: refusing to merge unrelated histories");
        return Err(GitError::Exit(128));
    }
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let other_tree = commit_tree_oid(&db, format, &other_oid)?;
    let ours_map = sley_diff_merge::flatten_tree(&db, format, &head_tree)?;
    let theirs_map = sley_diff_merge::flatten_tree(&db, format, &other_tree)?;

    let ours_label = "HEAD".to_string();
    let theirs_label = target.clone();
    let write_db = FileObjectDatabase::from_git_dir(&common_git_dir, format);

    // Recursive merge of the merge bases into a single virtual ancestor tree
    // (the merge-recursive "virtual ancestor" — git's behaviour for a
    // criss-cross history with >1 merge base). With a single base this is just
    // that base's tree, so the common case is unchanged.
    let base_map = if bases.is_empty() {
        // `--allow-unrelated-histories`: the two branches share no common
        // ancestor, so the merge base is the empty tree.
        MergeTreeMap::new()
    } else {
        virtual_ancestor_entry_map(&write_db, format, &bases, &common_git_dir)?
    };

    // `merge.conflictStyle`: diff3/zdiff3 add the `|||||||` common-ancestor
    // section to conflict markers (git honours this for `git merge`).
    let conflict_style = effective_config_with_overrides()
        .and_then(|config| {
            config
                .get("merge", None, "conflictstyle")
                .map(str::to_string)
        })
        .map(|value| match value.as_str() {
            "diff3" => sley_diff_merge::ConflictStyle::Diff3,
            "zdiff3" => sley_diff_merge::ConflictStyle::ZDiff3,
            _ => sley_diff_merge::ConflictStyle::Merge,
        })
        .unwrap_or(sley_diff_merge::ConflictStyle::Merge);
    // The diff3 ancestor label mirrors merge-ort's `ancestor_name`: "empty tree"
    // when there is no common ancestor, the merge base's abbreviated oid for a
    // unique base, and "merged common ancestors" for a recursive (multi-base)
    // merge. The abbreviation width matches `git rev-parse --short`.
    let ancestor_label = merge_diff3_ancestor_label(&common_git_dir, format, &bases);
    let attribute_favor = MergeAttributeFavorResolver::from_worktree_root(&worktree_root);
    let path_favor = |path: &[u8]| attribute_favor.favor_for_path(path);
    let path_is_binary = |path: &[u8]| attribute_favor.is_binary_for_path(path);
    let merge_outcome = three_way_merge_trees_outcome_with_info_opts_and_path_resolvers(
        &write_db,
        format,
        &base_map,
        &ours_map,
        &theirs_map,
        &ours_label,
        &theirs_label,
        &ancestor_label,
        options.favor,
        conflict_style,
        sley_diff_merge::WsIgnore::EMPTY,
        RenameMergeConfig {
            detect_renames: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: merge_rename_limit_config(),
            directory_renames: directory_renames_config(),
        },
        Some(&path_favor),
        None,
        Some(&path_is_binary),
    )?;
    let auto_merge_tree = merge_outcome.tree;
    let mut results = merge_outcome.results;
    let mut conflicts = merge_outcome.conflicts;
    let info_messages = merge_outcome.info_messages;
    resolve_trivial_submodule_conflicts(&worktree_root, format, &mut results, &mut conflicts)?;

    // git's pre-merge `verify_uptodate` (unpack-trees): a real 3-way merge
    // requires a clean starting state. Refuse — without writing any MERGE_HEAD —
    // if the index has staged changes vs HEAD, or if the worktree has local
    // modifications to a path the merge would overwrite. Untouched local
    // modifications are allowed (and preserved). This is the guard behind the
    // t7611 "merge ... fails" cases.
    verify_merge_uptodate(&worktree_root, &git_dir, format, &results, &ours_map)?;

    let target_map = merge_results_entry_map(&results);
    merge_refuse_if_current_working_directory_becomes_file(&worktree_root, &target_map)?;

    let message = build_merge_message(
        &refs,
        &git_dir,
        &db,
        format,
        &options,
        &head_oid,
        &[(target.clone(), other_oid)],
    )?;

    if conflicts.is_empty() {
        // Build the merged tree via a temporary stage-0 index, then commit + sync.
        let mut entries = Vec::new();
        for (path, result) in &results {
            if let MergePathResult::Resolved(Some((mode, oid))) = result {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let merged_paths: Vec<Vec<u8>> = entries.iter().map(|entry| entry.path.to_vec()).collect();
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

        // Materialize the merged result into the worktree (shared by the
        // --squash and --no-commit early-exit paths below). git's unpack-trees
        // only touches paths the merge CHANGED relative to HEAD; a path whose
        // merged result equals HEAD's entry is left exactly as-is, so a purely
        // local (unstaged) modification to an untouched file is preserved.
        let write_merged_worktree = || -> Result<()> {
            // Apply removals before additions. A directory->gitlink transition
            // has flattened delete entries below `path/` plus a gitlink at
            // `path`; writing the gitlink directory first and then pruning its
            // deleted children removes the newly-created empty directory.
            for (path, result) in &results {
                if matches!(result, MergePathResult::Resolved(None)) && ours_map.contains_key(path)
                {
                    merge_remove_worktree_file(&worktree_root, path)?;
                }
            }
            for path in ours_map.keys() {
                if !merged_paths.iter().any(|merged| merged == path) {
                    merge_remove_worktree_file(&worktree_root, path)?;
                }
            }
            for (path, result) in &results {
                if let MergePathResult::Resolved(Some(entry @ (mode, oid))) = result {
                    if ours_map.get(path) == Some(entry) {
                        continue;
                    }
                    let content = merge_worktree_content(&db, *mode, oid)?;
                    merge_write_worktree_file(&worktree_root, path, &content, *mode)?;
                }
            }
            Ok(())
        };

        // `--squash`: leave the merged result staged + in the worktree and write
        // SQUASH_MSG, but record NO in-progress merge (no MERGE_HEAD) and do not
        // move HEAD. git prints the clean-merge notice then the squash line.
        if options.squash {
            write_merged_worktree()?;
            refresh_merged_index_stat(&git_dir, &worktree_root, format)?;
            write_squash_message(&git_dir, &db, format, &head_oid, &other_oid)?;
            if !options.quiet {
                println!("Automatic merge went well; stopped before committing as requested");
                println!("Squash commit -- not updating HEAD");
            }
            commands::hooks::run_hook_l("post-merge", &["1"])?;
            if merge_autostash {
                apply_merge_autostash(&git_dir, format);
            }
            return Ok(());
        }

        if options.no_commit {
            write_merge_state(
                &git_dir,
                &[other_oid],
                merge_msg_file_contents(&message),
                &options,
                None,
            )?;
            if merge_autostash {
                write_merge_autostash_marker(&git_dir)?;
            }
            write_merged_worktree()?;
            refresh_merged_index_stat(&git_dir, &worktree_root, format)?;
            if !options.quiet {
                println!("Automatic merge went well; stopped before committing as requested");
            }
            return Ok(());
        }

        if !options.quiet {
            let mut stdout = io::stdout();
            let strategy = if options.resolve_strategy {
                "resolve"
            } else {
                "ort"
            };
            print_merge_info_messages(&info_messages);
            if options.resolve_strategy {
                writeln!(stdout, "Wonderful.")?;
            }
            writeln!(stdout, "Merge made by the '{strategy}' strategy.")?;
            write_merge_result_diffstat(
                &mut stdout,
                &db,
                format,
                &head_tree,
                &merged_tree,
                merge_diffstat_mode(&options),
            )?;
            stdout.flush()?;
        }
        if options.edit == Some(true) {
            write_merge_state(
                &git_dir,
                &[other_oid],
                merge_msg_file_contents(&message),
                &options,
                Some(&head_oid),
            )?;
            if merge_autostash {
                write_merge_autostash_marker(&git_dir)?;
            }
            write_merged_worktree()?;
        }
        let merged_oid = merge_commit_and_advance(
            &git_dir,
            &refs,
            format,
            &head_oid,
            &other_oid,
            merged_tree,
            prepare_merge_commit_message(&git_dir, &message, &options)?,
            &options,
        )?;
        if options.edit == Some(true) {
            clear_in_progress_merge_state(&git_dir);
        }
        // Remove file ancestors that the merged result replaces with
        // directories before writing changed paths. The ordinary merge path
        // below is deliberately incremental: a full reset after committing
        // would rewrite every tracked file, losing Git's unpack-trees promise
        // that entries unchanged from HEAD retain their inode timestamps.
        clear_merge_df_blockers(&worktree_root, &results);
        write_merged_worktree()?;
        if options.recurse_submodules {
            // Recursive submodule checkout has additional embedded-repository
            // movement semantics; keep using its dedicated reset path.
            reset_index_and_worktree_to_commit_for_merge(
                &worktree_root,
                &git_dir,
                format,
                &merged_oid,
                true,
            )?;
        } else {
            refresh_merged_index_stat(&git_dir, &worktree_root, format)?;
        }
        commands::hooks::run_hook_l("post-merge", &["0"])?;
        if merge_autostash {
            apply_merge_autostash(&git_dir, format);
        }
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
            // A directory-rename location/implicit-collision advisory is staged
            // cleanly at stage 0 (the path content is fully resolved); the
            // conflict is purely a message + nonzero exit, not an unmerged entry.
            MergePathResult::Conflict {
                ours,
                kind:
                    Some(
                        sley_diff_merge::MergeConflictKind::DirRenameLocation {
                            back_to_self: false,
                            ..
                        }
                        | sley_diff_merge::MergeConflictKind::DirRenameImplicitCollision { .. },
                    ),
                ..
            } => {
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 0));
                }
            }
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
    if options.resolve_strategy {
        for (path, (base_mode, base_oid)) in &base_map {
            if ours_map.contains_key(path)
                || entries.iter().any(|entry| entry.path.as_ref() == path)
            {
                continue;
            }
            if let Some((theirs_mode, theirs_oid)) = theirs_map.get(path) {
                entries.push(merge_index_entry(path, *base_mode, *base_oid, 1));
                entries.push(merge_index_entry(path, *theirs_mode, *theirs_oid, 3));
            }
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags >> 12).cmp(&(right.flags >> 12)))
    });
    // The index is written AFTER the worktree materialization below so freshly
    // resolved stage-0 entries can record their on-disk stat (git refreshes
    // cleanly-merged results via fill_stat_cache_info; a zeroed stat makes
    // diff-files report the resolved path as modified). Conflict stages (1/2/3)
    // keep zero stat, as git does.

    // Materialize merged/conflicted content into the worktree. Conflict entries
    // below a populated HEAD gitlink are superproject index state only: writing
    // them would dirty the submodule checkout itself.
    let populated_ours_gitlink_prefixes =
        populated_gitlink_directory_prefixes(&worktree_root, &ours_map)?;
    let materialization_config = effective_config_with_overrides().unwrap_or_default();
    for (path, result) in &results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = merge_worktree_content(&db, *mode, oid)?;
                    let (worktree_mode, content) = merge_materialized_content(
                        &worktree_root,
                        &git_dir,
                        format,
                        &materialization_config,
                        path,
                        *mode,
                        &content,
                    )?;
                    if materialize_gitlink_child_conflict_file(
                        &worktree_root,
                        path,
                        &populated_ours_gitlink_prefixes,
                        &theirs_label,
                        &content,
                        worktree_mode,
                    )? {
                        continue;
                    }
                    merge_write_worktree_file(&worktree_root, path, &content, worktree_mode)?;
                }
            }
            MergePathResult::Resolved(None) => {
                if path_is_inside_populated_gitlink(path, &populated_ours_gitlink_prefixes) {
                    continue;
                }
                // git only removes a worktree file when its content is the tracked
                // (ours/HEAD) version; an untracked file or one with divergent
                // content at this path is left alone (the rename/delete "Gollum's
                // ring" safety case). When the path was not in ours, or the file
                // on disk differs from ours' blob, preserve it.
                if worktree_file_matches_ours(&db, &worktree_root, path, ours_map.get(path))? {
                    merge_remove_worktree_file(&worktree_root, path)?;
                }
            }
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    let (worktree_mode, content) = merge_materialized_content(
                        &worktree_root,
                        &git_dir,
                        format,
                        &materialization_config,
                        path,
                        *mode,
                        content,
                    )?;
                    if conflict_worktree_matches_ours(
                        &db,
                        &worktree_root,
                        path,
                        worktree_mode,
                        &content,
                        ours_map.get(path),
                    )? {
                        continue;
                    }
                    if materialize_gitlink_child_conflict_file(
                        &worktree_root,
                        path,
                        &populated_ours_gitlink_prefixes,
                        &theirs_label,
                        &content,
                        worktree_mode,
                    )? {
                        continue;
                    }
                    merge_write_worktree_file(&worktree_root, path, &content, worktree_mode)?
                }
                None if matches!(
                    result,
                    MergePathResult::Conflict {
                        kind: Some(sley_diff_merge::MergeConflictKind::DirRenameSplit { .. }),
                        ..
                    }
                ) => {}
                None => {
                    if path_is_inside_populated_gitlink(path, &populated_ours_gitlink_prefixes) {
                        continue;
                    }
                    if worktree_file_matches_ours(&db, &worktree_root, path, ours_map.get(path))? {
                        merge_remove_worktree_file(&worktree_root, path)?;
                    }
                }
            },
        }
    }

    // Record the on-disk stat for cleanly-resolved stage-0 entries now that the
    // worktree holds their content (git's fill_stat_cache_info). Conflict stages
    // and gitlinks keep zero stat.
    for entry in &mut entries {
        if (entry.flags >> 12) & 0x3 != 0 || sley_index::is_gitlink(entry.mode) {
            continue;
        }
        if let Ok(rel) = std::str::from_utf8(entry.path.as_bytes())
            && let Ok(metadata) = fs::symlink_metadata(worktree_root.join(rel))
        {
            sley_worktree::fill_index_entry_stat_cache(entry, &metadata);
        }
    }
    fs::write(
        sley_worktree::repository_index_path(&git_dir),
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    write_auto_merge_ref(&git_dir, &auto_merge_tree)?;

    // The `# Conflicts:` trailer git appends to MERGE_MSG / SQUASH_MSG.
    let conflicts_block = merge_conflicts_block(&conflicts, false);
    let merge_msg_conflicts_block =
        merge_conflicts_block(&conflicts, merge_conflict_cleanup_scissors(&options));

    // `--squash` with conflicts: git writes SQUASH_MSG (the squash commit list,
    // NO conflict trailer) and a separate MERGE_MSG carrying just the
    // `# Conflicts:` block, but records NO in-progress merge (no MERGE_HEAD/
    // MERGE_MODE). A later `git commit` concatenates SQUASH_MSG + MERGE_MSG. The
    // `Squash commit -- not updating HEAD` notice precedes the failure line.
    if options.squash {
        write_squash_message(&git_dir, &db, format, &head_oid, &other_oid)?;
        fs::write(git_dir.join("MERGE_MSG"), &conflicts_block)?;
        print_merge_info_messages(&info_messages);
        print_merge_conflict_messages(&worktree_root, format, &results);
        println!("Squash commit -- not updating HEAD");
        if merge_autostash {
            save_squash_conflict_autostash(&git_dir, format);
        }
        println!("Automatic merge failed; fix conflicts and then commit the result.");
        return Err(GitError::Exit(1));
    }

    let mut merge_state_message = message;
    merge_state_message.push(b'\n');
    merge_state_message.extend_from_slice(merge_msg_conflicts_block.as_bytes());
    write_merge_state(&git_dir, &[other_oid], merge_state_message, &options, None)?;
    run_rerere_after_conflicted_merge(&git_dir, format, &options)?;
    if merge_autostash {
        write_merge_autostash_marker(&git_dir)?;
    }
    fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;

    print_merge_info_messages(&info_messages);
    print_merge_conflict_messages(&worktree_root, format, &results);
    println!("Automatic merge failed; fix conflicts and then commit the result.");
    Err(GitError::Exit(1))
}

/// git's `parse_rename_score` (`diff.c`): parse a `--find-renames`/
/// `--rename-threshold` argument such as `25%`, `100%`, `0.5`, or a bare number
/// into a similarity threshold *percentage* (`0..=100`). Returns `None` for
/// anything git rejects (a non-numeric body, a trailing garbage character, an
/// empty string) — exactly the leftover-character check git applies via
/// `*arg != 0`.
///
/// git accumulates `num`/`scale` over the digits (capping `scale` at 100000),
/// resets `scale` on a single `.`, folds a trailing `%`, and computes an internal
/// score out of `MAX_SCORE` (60000): `num >= scale` saturates to `MAX_SCORE`,
/// otherwise `MAX_SCORE * num / scale`. We then map that score to the engine's
/// percentage threshold by `score / 600` (the inverse of the engine's reported
/// similarity), which reproduces git's `score >= minimum_score` comparison at the
/// multiples-of-600 boundaries the thresholds resolve to.
fn parse_rename_score_threshold(arg: &str) -> Option<u8> {
    let bytes = arg.as_bytes();
    let mut num: u64 = 0;
    let mut scale: u64 = 1;
    let mut dot = false;
    let mut idx = 0;
    while idx < bytes.len() {
        let ch = bytes[idx];
        if !dot && ch == b'.' {
            scale = 1;
            dot = true;
        } else if ch == b'%' {
            scale = if dot { scale * 100 } else { 100 };
            idx += 1; // `%` is always the last character.
            break;
        } else if ch.is_ascii_digit() {
            if scale < 100000 {
                scale *= 10;
                num = num * 10 + u64::from(ch - b'0');
            }
        } else {
            break;
        }
        idx += 1;
    }
    // git's caller rejects the option unless the whole argument was consumed.
    if idx != bytes.len() {
        return None;
    }
    const MAX_SCORE: u64 = 60000;
    let score = if num >= scale {
        MAX_SCORE
    } else {
        MAX_SCORE * num / scale
    };
    Some((score / 600).min(100) as u8)
}

/// `git config_rename` truthiness for `diff.renames`/`merge.renames`: `copies`/
/// `copy` and any truthy boolean enable rename detection; an explicit false
/// disables it.
fn config_rename_enabled(value: &str) -> bool {
    if value.eq_ignore_ascii_case("copies") || value.eq_ignore_ascii_case("copy") {
        return true;
    }
    parse_maybe_bool(value).unwrap_or(true)
}

/// Resolve the default rename-detection enablement from config, mirroring
/// The diff3 common-ancestor label for a two-head merge, mirroring merge-ort's
/// `ancestor_name`: "empty tree" with no common ancestor, the unique merge
/// base's abbreviated oid (same width as `git rev-parse --short`), or "merged
/// common ancestors" for a recursive merge over several bases.
fn merge_diff3_ancestor_label(git_dir: &Path, format: ObjectFormat, bases: &[ObjectId]) -> String {
    match bases {
        [] => "empty tree".to_string(),
        [base] => {
            let hex = base.to_hex();
            let width = crate::repository_abbrev(git_dir, format)
                .ok()
                .flatten()
                .unwrap_or_else(|| format.hex_len());
            hex[..width.min(hex.len())].to_string()
        }
        _ => "merged common ancestors".to_string(),
    }
}

/// merge-ort's `merge_recursive_config`: `diff.renames` seeds it, then
/// `merge.renames` overrides. Unset → `true`.
fn merge_recursive_renames_default() -> bool {
    let Some(config) = effective_config_with_overrides() else {
        return true;
    };
    let mut detect = true;
    if let Some(value) = config.get("diff", None, "renames") {
        detect = config_rename_enabled(value);
    }
    if let Some(value) = config.get("merge", None, "renames") {
        detect = config_rename_enabled(value);
    }
    detect
}

/// `git merge-recursive <base>... -- <head> <remote>`: a 3-way merge with an
/// explicitly-given ancestor (or several, folded into a virtual ancestor),
/// writing the result into the *index* (stage 0 when resolved, stages 1/2/3 when
/// conflicted) and the worktree. Unlike `git merge` it never computes a merge
/// base, never touches HEAD/MERGE_HEAD, and never commits. Exit 0 on a clean
/// merge, 1 when any path conflicts.
///
/// It honours the rename-detection knobs `--find-renames[=<n>]`,
/// `--rename-threshold=<n>`, and `--no-renames` (last wins, left to right),
/// falling back to `merge.renames`/`diff.renames` config when none is given.
pub(crate) fn cmd_merge_recursive(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;

    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        return Err(GitError::Command(
            "merge-recursive requires '<base> -- <head> <remote>'".into(),
        ));
    };
    let head = args.get(separator + 1).ok_or_else(|| {
        GitError::Command("merge-recursive requires '<base> -- <head> <remote>'".into())
    })?;
    let remote = args.get(separator + 2).ok_or_else(|| {
        GitError::Command("merge-recursive requires '<base> -- <head> <remote>'".into())
    })?;

    // Options and the base list precede the `--`. Options begin with `-`; every
    // other token is a merge base. Rename settings follow git's last-wins order.
    let mut detect_renames = merge_recursive_renames_default();
    let mut rename_threshold = sley_diff_merge::DEFAULT_RENAME_THRESHOLD;
    let mut favor = sley_diff_merge::MergeFavor::None;
    let mut ws_ignore = sley_diff_merge::WsIgnore::EMPTY;
    let mut base_revs: Vec<&String> = Vec::new();
    for arg in &args[..separator] {
        if let Some(opt) = arg.strip_prefix("--") {
            if opt == "no-renames" {
                detect_renames = false;
            } else if opt == "find-renames" {
                // Bare form: enable detection and reset to the default threshold
                // (git stores rename_score 0, which diffcore maps to 50%).
                detect_renames = true;
                rename_threshold = sley_diff_merge::DEFAULT_RENAME_THRESHOLD;
            } else if let Some(value) = opt
                .strip_prefix("find-renames=")
                .or_else(|| opt.strip_prefix("rename-threshold="))
            {
                let Some(threshold) = parse_rename_score_threshold(value) else {
                    eprintln!("error: unknown option `{}'", &arg[2..]);
                    return Err(GitError::Exit(129));
                };
                detect_renames = true;
                rename_threshold = threshold;
            } else if opt == "ours" {
                favor = sley_diff_merge::MergeFavor::Ours;
            } else if opt == "theirs" {
                favor = sley_diff_merge::MergeFavor::Theirs;
            } else if opt == "ignore-space-change" {
                ws_ignore.space_change = true;
            } else if opt == "ignore-all-space" {
                ws_ignore.all_space = true;
            } else if opt == "ignore-space-at-eol" {
                ws_ignore.space_at_eol = true;
            } else if opt == "ignore-cr-at-eol" {
                ws_ignore.cr_at_eol = true;
            } else if matches!(
                opt,
                "renormalize" | "no-renormalize" | "patience" | "histogram" | "minimal" | "subtree"
            ) || opt.starts_with("diff-algorithm=")
                || opt.starts_with("subtree=")
            {
                // Accepted for compatibility; not material to the merge result here.
            } else {
                eprintln!("error: unknown option `{}'", &arg[2..]);
                return Err(GitError::Exit(129));
            }
        } else {
            base_revs.push(arg);
        }
    }

    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);

    // Resolve the explicit ancestor(s) into a single (possibly virtual) base tree.
    let base_map = if base_revs.is_empty() {
        MergeTreeMap::new()
    } else {
        let bases = base_revs
            .iter()
            .map(|rev| resolve_revision(&git_dir, format, rev))
            .collect::<Result<Vec<_>>>()?;
        virtual_ancestor_entry_map(&db, format, &bases, &common_git_dir)?
    };

    let head_oid = resolve_revision(&git_dir, format, head)?;
    let remote_oid = resolve_revision(&git_dir, format, remote)?;
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let remote_tree = commit_tree_oid(&db, format, &remote_oid)?;
    let ours_map = sley_diff_merge::flatten_tree(&db, format, &head_tree)?;
    let theirs_map = sley_diff_merge::flatten_tree(&db, format, &remote_tree)?;

    let attribute_favor = MergeAttributeFavorResolver::from_worktree_root(&worktree_root);
    let path_favor = |path: &[u8]| attribute_favor.favor_for_path(path);
    let (mut results, mut conflicts, info_messages) =
        three_way_merge_trees_inner_with_info_opts_and_path_favor(
            &db,
            format,
            &base_map,
            &ours_map,
            &theirs_map,
            head,
            remote,
            "merged common ancestors",
            favor,
            sley_diff_merge::ConflictStyle::Merge,
            ws_ignore,
            RenameMergeConfig {
                detect_renames,
                rename_threshold,
                rename_limit: merge_rename_limit_config(),
                directory_renames: directory_renames_config(),
            },
            Some(&path_favor),
        )?;
    resolve_trivial_submodule_conflicts(&worktree_root, format, &mut results, &mut conflicts)?;

    write_merge_recursive_index(&git_dir, format, &results)?;
    apply_merge_recursive_worktree(&db, &worktree_root, &results, &ours_map)?;

    print_merge_info_messages(&info_messages);
    print_merge_conflict_messages(&worktree_root, format, &results);

    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

/// Write the result of a `merge-recursive` run into the repository index: stage 0
/// for resolved paths (and for the advisory directory-rename location/collision
/// conflicts, which git stages cleanly), stages 1/2/3 for genuine conflicts.
fn write_merge_recursive_index(
    git_dir: &Path,
    format: ObjectFormat,
    results: &MergePathResults,
) -> Result<()> {
    let mut entries = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict {
                ours,
                kind:
                    Some(
                        sley_diff_merge::MergeConflictKind::DirRenameLocation {
                            back_to_self: false,
                            ..
                        }
                        | sley_diff_merge::MergeConflictKind::DirRenameImplicitCollision { .. },
                    ),
                ..
            } => {
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 0));
                }
            }
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
    Ok(())
}

/// Materialize a `merge-recursive` result into the worktree: write resolved/
/// conflicted content, and remove paths the merge dropped when the worktree still
/// holds the tracked (ours) version (git's rename/delete "Gollum's ring" safety).
fn apply_merge_recursive_worktree(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    results: &MergePathResults,
    ours_map: &MergeTreeMap,
) -> Result<()> {
    let populated_ours_gitlink_prefixes =
        populated_gitlink_directory_prefixes(worktree_root, ours_map)?;
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = merge_worktree_content(db, *mode, oid)?;
                    if materialize_gitlink_child_conflict_file(
                        worktree_root,
                        path,
                        &populated_ours_gitlink_prefixes,
                        "theirs",
                        &content,
                        *mode,
                    )? {
                        continue;
                    }
                    merge_write_worktree_file(worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => {
                if path_is_inside_populated_gitlink(path, &populated_ours_gitlink_prefixes) {
                    continue;
                }
                if worktree_file_matches_ours(db, worktree_root, path, ours_map.get(path))? {
                    merge_remove_worktree_file(worktree_root, path)?;
                }
            }
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    if conflict_worktree_matches_ours(
                        db,
                        worktree_root,
                        path,
                        *mode,
                        content,
                        ours_map.get(path),
                    )? {
                        continue;
                    }
                    if materialize_gitlink_child_conflict_file(
                        worktree_root,
                        path,
                        &populated_ours_gitlink_prefixes,
                        "theirs",
                        content,
                        *mode,
                    )? {
                        continue;
                    }
                    merge_write_worktree_file(worktree_root, path, content, *mode)?;
                }
                None if matches!(
                    result,
                    MergePathResult::Conflict {
                        kind: Some(sley_diff_merge::MergeConflictKind::DirRenameSplit { .. }),
                        ..
                    }
                ) => {}
                None => {
                    if path_is_inside_populated_gitlink(path, &populated_ours_gitlink_prefixes) {
                        continue;
                    }
                    if worktree_file_matches_ours(db, worktree_root, path, ours_map.get(path))? {
                        merge_remove_worktree_file(worktree_root, path)?;
                    }
                }
            },
        }
    }
    Ok(())
}

/// True when materializing a conflicted path would only rewrite the exact file
/// already checked out from HEAD. Merge-ort leaves that path alone (notably a
/// modify/delete conflict where ours is the surviving side), preserving both
/// local safety and the worktree timestamp.
fn conflict_worktree_matches_ours(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    path: &[u8],
    intended_mode: u32,
    intended_content: &[u8],
    ours: Option<&(u32, ObjectId)>,
) -> Result<bool> {
    let Some((ours_mode, ours_oid)) = ours else {
        return Ok(false);
    };
    if *ours_mode != intended_mode || !merge_worktree_path_exists(worktree_root, path) {
        return Ok(false);
    }
    let ours_content = merge_worktree_content(db, *ours_mode, ours_oid)?;
    if ours_content != intended_content {
        return Ok(false);
    }
    worktree_file_matches_ours(db, worktree_root, path, ours)
}

/// Convert an index/blob merge result into the bytes and filesystem type Git
/// writes to the worktree. Higher-order index stages keep their canonical modes
/// and blob bytes; only the worktree view is smudged or degraded to a regular
/// file when symlinks are disabled.
fn merge_materialized_content(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    path: &[u8],
    mode: u32,
    content: &[u8],
) -> Result<(u32, Vec<u8>)> {
    if sley_index::is_symlink_mode(mode) {
        let trust_symlinks = config.get_bool("core", None, "symlinks").unwrap_or(true);
        return Ok((
            if trust_symlinks { mode } else { 0o100644 },
            content.to_vec(),
        ));
    }
    if sley_index::is_gitlink(mode) {
        return Ok((mode, content.to_vec()));
    }
    let content =
        sley_worktree::apply_smudge_filter(worktree_root, git_dir, format, config, path, content)?;
    Ok((mode, content))
}

fn resolve_trivial_submodule_conflicts(
    worktree_root: &Path,
    format: ObjectFormat,
    results: &mut MergePathResults,
    conflicts: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let mut resolved = BTreeSet::new();
    for path in conflicts.iter() {
        let Some(entry) =
            trivial_submodule_conflict_resolution(worktree_root, format, path, results)
        else {
            continue;
        };
        results.insert(path.to_vec(), MergePathResult::Resolved(Some(entry)));
        resolved.insert(path.to_vec());
    }
    if !resolved.is_empty() {
        conflicts.retain(|path| !resolved.contains(path));
    }
    Ok(())
}

fn trivial_submodule_conflict_resolution(
    worktree_root: &Path,
    format: ObjectFormat,
    path: &[u8],
    results: &MergePathResults,
) -> Option<(u32, ObjectId)> {
    let MergePathResult::Conflict {
        base, ours, theirs, ..
    } = results.get(path)?
    else {
        return None;
    };
    let Some((ours_mode, ours_oid)) = ours else {
        return None;
    };
    let Some((theirs_mode, theirs_oid)) = theirs else {
        return None;
    };
    if !sley_index::is_gitlink(*ours_mode) || !sley_index::is_gitlink(*theirs_mode) {
        return None;
    }
    let sub_root = worktree_root.join(repo_path_to_path(path));
    let sub_git_dir = sley_diff_merge::gitlink_git_dir(&sub_root)?;
    let sub_format = repository_object_format(&sub_git_dir)
        .ok()
        .unwrap_or(format);
    let sub_db = FileObjectDatabase::from_git_dir(&sub_git_dir, sub_format);
    let Some((base_mode, base_oid)) = base else {
        return None;
    };
    if !sley_index::is_gitlink(*base_mode)
        || !submodule_commit_is_ancestor(&sub_git_dir, &sub_db, sub_format, base_oid, ours_oid)
        || !submodule_commit_is_ancestor(&sub_git_dir, &sub_db, sub_format, base_oid, theirs_oid)
    {
        return None;
    }
    if submodule_commit_is_ancestor(&sub_git_dir, &sub_db, sub_format, ours_oid, theirs_oid) {
        Some((*theirs_mode, *theirs_oid))
    } else if submodule_commit_is_ancestor(&sub_git_dir, &sub_db, sub_format, theirs_oid, ours_oid)
    {
        Some((*ours_mode, *ours_oid))
    } else {
        None
    }
}

fn submodule_commit_is_ancestor(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    ancestor: &ObjectId,
    descendant: &ObjectId,
) -> bool {
    ancestor == descendant
        || sley_rev::merge_bases(git_dir, format, db, ancestor, descendant)
            .ok()
            .is_some_and(|bases| bases.iter().any(|base| base == ancestor))
}

fn print_merge_info_messages(messages: &[sley_diff_merge::MergeInfoMessage]) {
    for message in messages {
        match message {
            sley_diff_merge::MergeInfoMessage::AutoMerge { path } => {
                println!("Auto-merging {}", String::from_utf8_lossy(path));
            }
            sley_diff_merge::MergeInfoMessage::DirRenameSkippedDueToRerename {
                old_dir,
                path,
                new_dir,
            } => {
                println!(
                    "WARNING: Avoiding applying {} -> {} rename to {}, because {} itself was renamed.",
                    String::from_utf8_lossy(old_dir),
                    String::from_utf8_lossy(new_dir),
                    String::from_utf8_lossy(path),
                    String::from_utf8_lossy(new_dir),
                );
            }
            sley_diff_merge::MergeInfoMessage::DirRenameApplied {
                old_path,
                new_path,
                renamed_from,
                added_in,
                dir_renamed_in,
            } => match renamed_from {
                Some(source) => println!(
                    "Path updated: {} renamed to {} in {}, inside a directory that was renamed in {}; moving it to {}.",
                    String::from_utf8_lossy(source),
                    String::from_utf8_lossy(old_path),
                    added_in,
                    dir_renamed_in,
                    String::from_utf8_lossy(new_path),
                ),
                None => println!(
                    "Path updated: {} added in {} inside a directory that was renamed in {}; moving it to {}.",
                    String::from_utf8_lossy(old_path),
                    added_in,
                    dir_renamed_in,
                    String::from_utf8_lossy(new_path),
                ),
            },
            sley_diff_merge::MergeInfoMessage::DirRenameLocationConflict {
                old_path,
                new_path,
                renamed_from,
                added_in,
                dir_renamed_in,
            } => match renamed_from {
                Some(source) => println!(
                    "CONFLICT (file location): {} renamed to {} in {}, inside a directory that was renamed in {}, suggesting it should perhaps be moved to {}.",
                    String::from_utf8_lossy(source),
                    String::from_utf8_lossy(old_path),
                    added_in,
                    dir_renamed_in,
                    String::from_utf8_lossy(new_path),
                ),
                None => println!(
                    "CONFLICT (file location): {} added in {} inside a directory that was renamed in {}, suggesting it should perhaps be moved to {}.",
                    String::from_utf8_lossy(old_path),
                    added_in,
                    dir_renamed_in,
                    String::from_utf8_lossy(new_path),
                ),
            },
            sley_diff_merge::MergeInfoMessage::RenameDeleteConflict {
                old_path,
                new_path,
                renamed_in,
                deleted_in,
            } => {
                println!(
                    "CONFLICT (rename/delete): {} renamed to {} in {renamed_in}, but deleted in {deleted_in}.",
                    String::from_utf8_lossy(old_path),
                    String::from_utf8_lossy(new_path),
                );
            }
            sley_diff_merge::MergeInfoMessage::ModifyDeleteConflict {
                path,
                deleted_in,
                modified_in,
            } => {
                println!(
                    "CONFLICT (modify/delete): {} deleted in {deleted_in} and modified in {modified_in}.  Version {modified_in} of {} left in tree.",
                    String::from_utf8_lossy(path),
                    String::from_utf8_lossy(path),
                );
            }
        }
    }
}

/// Emit git's per-path merge conflict notices, in path order, from the reshaped
/// merge results. Mirrors merge-ort's `path_msg` set: an `Auto-merging <path>`
/// info line precedes the `CONFLICT (…)` line for any path that went through a
/// textual 3-way merge, and each conflict kind renders its own message. The
/// `results` map is keyed by path so iteration is already sorted like git's
/// message ordering.
fn print_merge_conflict_messages(
    worktree_root: &Path,
    format: ObjectFormat,
    results: &MergePathResults,
) {
    for (path, result) in results {
        let MergePathResult::Conflict {
            kind, auto_merged, ..
        } = result
        else {
            continue;
        };
        let path_str = String::from_utf8_lossy(path);
        if let Some(advice) = merge_submodule_conflict_advice(worktree_root, format, path, result) {
            for candidate in &advice.candidates {
                println!("Possible submodule merge resolution for {path_str}: {candidate}");
            }
            eprintln!("Failed to merge submodule {path_str}");
            eprintln!("CONFLICT (submodule): Merge conflict in {path_str}");
            eprintln!("Recursive merging with submodules currently only supports trivial cases.");
            eprintln!("Please manually handle the merging of each conflicted submodule.");
            eprintln!("This can be accomplished with the following steps:");
            eprintln!(
                " - go to submodule ({path_str}), and either merge commit {}",
                advice.theirs
            );
            eprintln!("   or update to an existing commit which has merged those changes");
            eprintln!(" - come back to superproject and run:");
            eprintln!("      git add {path_str}");
            eprintln!("   to record the above merge or update");
            eprintln!(" - resolve any other conflicts in the superproject");
            eprintln!(" - commit the resulting index in the superproject");
            continue;
        }
        if *auto_merged {
            println!("Auto-merging {path_str}");
        }
        match kind {
            Some(sley_diff_merge::MergeConflictKind::Content { add_add }) => {
                let reason = if *add_add { "add/add" } else { "content" };
                println!("CONFLICT ({reason}): Merge conflict in {path_str}");
            }
            Some(sley_diff_merge::MergeConflictKind::RenameContent { .. }) => {
                println!("CONFLICT (content): Merge conflict in {path_str}");
            }
            Some(sley_diff_merge::MergeConflictKind::RenameRenameTwoToOne {
                ours_path,
                theirs_path,
            }) => {
                println!(
                    "CONFLICT (rename/rename): {} and {} renamed to {path_str}, respectively.",
                    String::from_utf8_lossy(ours_path),
                    String::from_utf8_lossy(theirs_path),
                );
            }
            Some(sley_diff_merge::MergeConflictKind::RenameRenameOneToTwo {
                old_path,
                ours_path,
                theirs_path,
                ours_label,
                theirs_label,
            }) => {
                println!(
                    "CONFLICT (rename/rename): {} renamed to {} in {ours_label} and to {} in {theirs_label}.",
                    String::from_utf8_lossy(old_path),
                    String::from_utf8_lossy(ours_path),
                    String::from_utf8_lossy(theirs_path),
                );
            }
            Some(sley_diff_merge::MergeConflictKind::RenameRenameOneToTwoStage) => {}
            Some(sley_diff_merge::MergeConflictKind::DirRenameSplit { source_dir }) => {
                println!(
                    "CONFLICT (directory rename split): Unclear where to rename {} to; it was renamed to multiple other directories, with no destination getting a majority of the files.",
                    String::from_utf8_lossy(source_dir),
                );
            }
            Some(sley_diff_merge::MergeConflictKind::ModifyDelete {
                deleted_in,
                modified_in,
            }) => {
                println!(
                    "CONFLICT (modify/delete): {path_str} deleted in {deleted_in} and modified in {modified_in}.  Version {modified_in} of {path_str} left in tree."
                );
            }
            Some(sley_diff_merge::MergeConflictKind::RenameDelete {
                old_path,
                renamed_in,
                deleted_in,
            }) => {
                println!(
                    "CONFLICT (rename/delete): {} renamed to {path_str} in {renamed_in}, but deleted in {deleted_in}.",
                    String::from_utf8_lossy(old_path)
                );
            }
            Some(sley_diff_merge::MergeConflictKind::FileDirectory {
                original_path,
                moved_from,
            }) => {
                println!(
                    "CONFLICT (file/directory): directory in the way of {} from {moved_from}; moving it to {path_str} instead.",
                    String::from_utf8_lossy(original_path)
                );
            }
            Some(sley_diff_merge::MergeConflictKind::DirRenameLocation {
                old_path,
                renamed_from,
                added_in,
                dir_renamed_in,
                back_to_self: _,
            }) => match renamed_from {
                Some(source) => println!(
                    "CONFLICT (file location): {src} renamed to {old} in {added_in}, inside a directory that was renamed in {dir_renamed_in}, suggesting it should perhaps be moved to {path_str}.",
                    src = String::from_utf8_lossy(source),
                    old = String::from_utf8_lossy(old_path),
                ),
                None => println!(
                    "CONFLICT (file location): {old} added in {added_in} inside a directory that was renamed in {dir_renamed_in}, suggesting it should perhaps be moved to {path_str}.",
                    old = String::from_utf8_lossy(old_path),
                ),
            },
            Some(sley_diff_merge::MergeConflictKind::DirRenameImplicitCollision { sources }) => {
                let source_list = sources
                    .iter()
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                if sources.len() > 1 {
                    println!(
                        "CONFLICT (implicit dir rename): Cannot map more than one path to {path_str}; implicit directory renames tried to put these paths there: {source_list}"
                    );
                } else {
                    println!(
                        "CONFLICT (implicit dir rename): Existing file/dir at {path_str} in the way of implicit directory rename(s) putting the following path(s) there: {source_list}."
                    );
                }
            }
            Some(sley_diff_merge::MergeConflictKind::DistinctTypes {
                original_path,
                ours_renamed,
                theirs_renamed,
            }) => {
                let renamed_both = ours_renamed.is_some() && theirs_renamed.is_some();
                let which = if renamed_both { "both" } else { "one" };
                println!(
                    "CONFLICT (distinct types): {orig} had different types on each side; renamed {which} of them so each can be recorded somewhere.",
                    orig = String::from_utf8_lossy(original_path),
                );
            }
            Some(sley_diff_merge::MergeConflictKind::DistinctTypesStage) => {}
            None => {
                println!("CONFLICT (content): Merge conflict in {path_str}");
            }
        }
    }
}

struct MergeSubmoduleConflictAdvice {
    theirs: String,
    candidates: Vec<String>,
}

fn merge_submodule_conflict_advice(
    worktree_root: &Path,
    format: ObjectFormat,
    path: &[u8],
    result: &MergePathResult,
) -> Option<MergeSubmoduleConflictAdvice> {
    let MergePathResult::Conflict {
        base, ours, theirs, ..
    } = result
    else {
        return None;
    };
    if ![base, ours, theirs]
        .into_iter()
        .flatten()
        .any(|(mode, _)| sley_index::is_gitlink(*mode))
    {
        return None;
    }
    let (_, theirs_oid) = theirs.as_ref()?;
    let candidates = submodule_merge_resolution_candidates(
        worktree_root,
        format,
        path,
        ours.map(|(_, oid)| oid),
        *theirs_oid,
    )
    .into_iter()
    .map(|oid| short_oid(&oid))
    .collect();
    Some(MergeSubmoduleConflictAdvice {
        theirs: short_oid(theirs_oid),
        candidates,
    })
}

fn short_oid(oid: &ObjectId) -> String {
    oid.to_hex()[..oid.abbrev_hex_len(7)].to_string()
}

fn submodule_merge_resolution_candidates(
    worktree_root: &Path,
    format: ObjectFormat,
    path: &[u8],
    ours: Option<ObjectId>,
    theirs: ObjectId,
) -> Vec<ObjectId> {
    let Some(ours) = ours else {
        return Vec::new();
    };
    let sub_root = worktree_root.join(repo_path_to_path(path));
    let Some(sub_git_dir) = sley_diff_merge::gitlink_git_dir(&sub_root) else {
        return Vec::new();
    };
    let sub_format = repository_object_format(&sub_git_dir)
        .ok()
        .unwrap_or(format);
    let sub_db = FileObjectDatabase::from_git_dir(&sub_git_dir, sub_format);
    let refs = FileRefStore::new(&sub_git_dir, sub_format)
        .list_refs()
        .unwrap_or_default();
    let mut out = BTreeSet::new();
    for reference in refs {
        let sley_refs::RefTarget::Direct(candidate) = reference.target else {
            continue;
        };
        if candidate == ours || candidate == theirs {
            continue;
        }
        if submodule_commit_is_ancestor(&sub_git_dir, &sub_db, sub_format, &ours, &candidate)
            && submodule_commit_is_ancestor(&sub_git_dir, &sub_db, sub_format, &theirs, &candidate)
        {
            out.insert(candidate);
        }
    }
    out.into_iter().collect()
}

fn merge_conflicts_block(conflicts: &[Vec<u8>], scissors: bool) -> String {
    let mut out = String::new();
    if scissors {
        out.push_str(
            "\n# ------------------------ >8 ------------------------\n\
             # Do not modify or remove the line above.\n\
             # Everything below it will be ignored.\n\
             #\n",
        );
    } else {
        out.push('\n');
    }
    out.push_str("# Conflicts:\n");
    for path in conflicts {
        out.push_str(&format!("#\t{}\n", String::from_utf8_lossy(path)));
    }
    out
}

fn merge_conflict_cleanup_scissors(options: &MergeOptions) -> bool {
    if options.cleanup == Some(CommitCleanupMode::Scissors) {
        return true;
    }

    effective_config_with_overrides()
        .and_then(|config| {
            config
                .get("commit", None, "cleanup")
                .map(|value| value.trim().eq_ignore_ascii_case("scissors"))
        })
        .unwrap_or(false)
}

fn run_rerere_after_conflicted_merge(
    git_dir: &Path,
    format: ObjectFormat,
    options: &MergeOptions,
) -> Result<()> {
    if let Ok(mut suppress) = SUPPRESS_RERERE_ONCE.lock()
        && *suppress
    {
        *suppress = false;
        return Ok(());
    }
    if !commands::rerere::is_rerere_enabled(git_dir) {
        return Ok(());
    }
    commands::rerere::repo_rerere(git_dir, format, options.rerere_autoupdate).map(|_| ())
}

/// git's pre-merge `verify_uptodate` guard. Returns an error (exit 2, matching
/// git's `ret = 2` for "local changes would be overwritten") when the worktree
/// is not a clean base for a real 3-way merge:
///   * any path is staged differently from HEAD (`index` status non-blank), or
///   * a path the merge would change relative to HEAD has an unstaged worktree
///     modification.
/// Purely-local modifications to paths the merge leaves alone are permitted.
fn verify_merge_uptodate(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    results: &MergePathResults,
    ours_map: &MergeTreeMap,
) -> Result<()> {
    // Paths whose merged result differs from HEAD (i.e. the merge touches them).
    let mut changed: BTreeSet<Vec<u8>> = BTreeSet::new();
    for (path, result) in results {
        let differs = match result {
            MergePathResult::Resolved(Some(entry)) => ours_map.get(path) != Some(entry),
            MergePathResult::Resolved(None) => ours_map.contains_key(path),
            MergePathResult::Conflict { .. } => true,
        };
        if differs {
            changed.insert(path.clone());
        }
    }
    // A HEAD path that the merge result no longer carries was vacated — e.g. a
    // directory rename moved `z/c` to `y/c`, so the merge deletes `z/c`. Such a
    // path is "changed" even though it never appears as a result entry, and a
    // dirty worktree file there must still trip the uptodate guard (t6423 11b/d).
    for path in ours_map.keys() {
        let carried = matches!(results.get(path), Some(MergePathResult::Resolved(Some(_))));
        if !carried {
            changed.insert(path.clone());
        }
    }

    let conflicted_gitlinks = conflicted_gitlink_paths(results, ours_map);
    let target_map = merge_results_entry_map(results);
    verify_no_populated_gitlink_directory_overwrite(
        worktree_root,
        &target_map,
        ours_map,
        &conflicted_gitlinks,
    )?;

    let status = crate::collect_short_status(worktree_root, git_dir, format)?;
    for entry in &status {
        let gitlink_worktree_status_is_safe = (conflicted_gitlinks.contains(&entry.path)
            || ours_map
                .get(&entry.path)
                .is_some_and(|(mode, _)| sley_index::is_gitlink(*mode)))
            && changed.contains(&entry.path);
        if entry.index == b'?'
            && entry.worktree == b'?'
            && changed.contains(&entry.path)
            && !gitlink_worktree_status_is_safe
        {
            eprintln!(
                "error: The following untracked working tree files would be overwritten by merge:\n\t{}",
                String::from_utf8_lossy(&entry.path)
            );
            eprintln!("Please move or remove them before you merge.");
            eprintln!("Aborting");
            return Err(GitError::Exit(1));
        }
        // A staged change anywhere (index column non-blank, not untracked/ignored)
        // makes the index an unclean merge base.
        if entry.index != b' ' && entry.index != b'?' && entry.index != b'!' {
            let staged_superproject_change =
                entry.head_mode != entry.index_mode || entry.head_oid != entry.index_oid;
            let gitlink_index_status_is_worktree_dirt = ours_map
                .get(&entry.path)
                .is_some_and(|(mode, _)| sley_index::is_gitlink(*mode))
                && !staged_superproject_change;
            if gitlink_index_status_is_worktree_dirt {
                continue;
            }
            eprintln!(
                "error: Your local changes to the following files would be overwritten by merge:\n  {}",
                String::from_utf8_lossy(&entry.path)
            );
            eprintln!("Please commit your changes or stash them before you merge.");
            eprintln!("Aborting");
            return Err(GitError::Exit(2));
        }
        // An unstaged worktree modification to a path the merge would change.
        if entry.worktree != b' '
            && entry.worktree != b'?'
            && entry.worktree != b'!'
            && changed.contains(&entry.path)
            && !gitlink_worktree_status_is_safe
        {
            eprintln!(
                "error: Your local changes to the following files would be overwritten by merge:\n  {}",
                String::from_utf8_lossy(&entry.path)
            );
            eprintln!("Please commit your changes or stash them before you merge.");
            eprintln!("Aborting");
            return Err(GitError::Exit(2));
        }
    }
    Ok(())
}

fn conflicted_gitlink_paths(
    results: &MergePathResults,
    ours_map: &MergeTreeMap,
) -> BTreeSet<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for (path, result) in results {
        let MergePathResult::Conflict {
            base,
            ours,
            theirs,
            kind,
            ..
        } = result
        else {
            continue;
        };
        if [base, ours, theirs]
            .iter()
            .any(|entry| entry.is_some_and(|(mode, _)| sley_index::is_gitlink(mode)))
        {
            paths.insert(path.clone());
        }
        if let Some(
            sley_diff_merge::MergeConflictKind::FileDirectory { original_path, .. }
            | sley_diff_merge::MergeConflictKind::DistinctTypes { original_path, .. },
        ) = kind
            && ours_map
                .get(original_path)
                .is_some_and(|(mode, _)| sley_index::is_gitlink(*mode))
        {
            paths.insert(original_path.clone());
        }
    }
    paths
}

pub(crate) fn verify_fast_forward_untracked_safe(
    worktree_root: &Path,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head_tree: &ObjectId,
    target_tree: &ObjectId,
) -> Result<()> {
    let head_map = sley_diff_merge::flatten_tree(db, format, head_tree)?;
    let target_map = sley_diff_merge::flatten_tree(db, format, target_tree)?;
    verify_no_populated_gitlink_directory_overwrite(
        worktree_root,
        &target_map,
        &head_map,
        &BTreeSet::new(),
    )?;
    let status = crate::collect_short_status(worktree_root, git_dir, format)?;
    let untracked: BTreeSet<Vec<u8>> = status
        .iter()
        .filter(|entry| entry.index == b'?' && entry.worktree == b'?')
        .map(|entry| entry.path.clone())
        .collect();
    for path in target_map.keys() {
        if head_map.contains_key(path) {
            continue;
        }
        if target_map
            .get(path)
            .is_some_and(|(mode, _)| sley_index::is_gitlink(*mode))
            && gitlink_target_dir_is_safe(worktree_root, path, &head_map, &untracked)?
        {
            continue;
        }
        let rel = std::str::from_utf8(path)
            .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
        if worktree_root.join(rel).exists() {
            eprintln!(
                "error: The following untracked working tree files would be overwritten by merge:\n\t{}",
                String::from_utf8_lossy(path)
            );
            eprintln!("Please move or remove them before you merge.");
            eprintln!("Aborting");
            return Err(GitError::Exit(1));
        }
    }
    Ok(())
}

fn gitlink_target_dir_is_safe(
    worktree_root: &Path,
    path: &[u8],
    head_map: &MergeTreeMap,
    untracked: &BTreeSet<Vec<u8>>,
) -> Result<bool> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    let Ok(metadata) = fs::symlink_metadata(&full) else {
        return Ok(true);
    };
    if !metadata.is_dir() {
        return Ok(false);
    }
    if fs::read_dir(&full)?.next().is_none() {
        return Ok(true);
    }
    let prefix = path_with_trailing_slash(path);
    let tracked_dir = head_map
        .keys()
        .any(|candidate| candidate.starts_with(&prefix));
    if !tracked_dir {
        return Ok(false);
    }
    Ok(!untracked
        .iter()
        .any(|candidate| candidate == path || candidate.starts_with(&prefix)))
}

fn path_with_trailing_slash(path: &[u8]) -> Vec<u8> {
    let mut prefix = path.to_vec();
    prefix.push(b'/');
    prefix
}

fn populated_gitlink_directory_prefixes(
    worktree_root: &Path,
    map: &MergeTreeMap,
) -> Result<BTreeSet<Vec<u8>>> {
    let mut prefixes = BTreeSet::new();
    for (path, (mode, _)) in map {
        if !sley_index::is_gitlink(*mode) {
            continue;
        }
        let rel = std::str::from_utf8(path)
            .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
        if sley_diff_merge::gitlink_git_dir(&worktree_root.join(rel)).is_some() {
            prefixes.insert(path_with_trailing_slash(path));
        }
    }
    Ok(prefixes)
}

fn path_is_inside_populated_gitlink(path: &[u8], prefixes: &BTreeSet<Vec<u8>>) -> bool {
    prefixes.iter().any(|prefix| path.starts_with(prefix))
}

fn populated_gitlink_prefix_for_path<'a>(
    path: &[u8],
    prefixes: &'a BTreeSet<Vec<u8>>,
) -> Option<&'a [u8]> {
    prefixes
        .iter()
        .find(|prefix| path.starts_with(prefix.as_slice()))
        .map(Vec::as_slice)
}

fn materialize_gitlink_child_conflict_file(
    worktree_root: &Path,
    path: &[u8],
    prefixes: &BTreeSet<Vec<u8>>,
    label: &str,
    content: &[u8],
    mode: u32,
) -> Result<bool> {
    let Some(prefix) = populated_gitlink_prefix_for_path(path, prefixes) else {
        return Ok(false);
    };
    if merge_worktree_path_exists(worktree_root, path) || sley_index::is_gitlink(mode) {
        return Ok(true);
    }
    let alternate = gitlink_child_conflict_path(worktree_root, prefix, path, label);
    merge_write_worktree_file(worktree_root, &alternate, content, mode)?;
    Ok(true)
}

fn gitlink_child_conflict_path(
    worktree_root: &Path,
    prefix: &[u8],
    path: &[u8],
    label: &str,
) -> Vec<u8> {
    let gitlink_path = &prefix[..prefix.len().saturating_sub(1)];
    let child_path = &path[prefix.len()..];
    let mut base = gitlink_path.to_vec();
    base.push(b'~');
    base.extend_from_slice(flatten_merge_label(label).as_bytes());
    let mut candidate = append_gitlink_child_path(&base, child_path);
    if !merge_worktree_path_exists(worktree_root, &candidate) {
        return candidate;
    }
    let mut suffix = 0usize;
    loop {
        let mut suffixed = base.clone();
        suffixed.push(b'_');
        suffixed.extend_from_slice(suffix.to_string().as_bytes());
        candidate = append_gitlink_child_path(&suffixed, child_path);
        if !merge_worktree_path_exists(worktree_root, &candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn append_gitlink_child_path(base: &[u8], child: &[u8]) -> Vec<u8> {
    let mut path = base.to_vec();
    if !child.is_empty() {
        path.push(b'/');
        path.extend_from_slice(child);
    }
    path
}

fn flatten_merge_label(label: &str) -> String {
    label.replace('/', "_")
}

fn verify_no_populated_gitlink_directory_overwrite(
    worktree_root: &Path,
    target_map: &MergeTreeMap,
    head_map: &MergeTreeMap,
    conflicted_gitlinks: &BTreeSet<Vec<u8>>,
) -> Result<()> {
    for (path, (mode, _)) in head_map {
        if !sley_index::is_gitlink(*mode) {
            continue;
        }
        if conflicted_gitlinks.contains(path) && populated_gitlink_exists(worktree_root, path)? {
            continue;
        }
        let prefix = path_with_trailing_slash(path);
        let overwritten: Vec<&Vec<u8>> = target_map
            .keys()
            .filter(|candidate| candidate.starts_with(&prefix))
            .filter(|candidate| merge_worktree_path_exists(worktree_root, candidate))
            .collect();
        if overwritten.is_empty() {
            continue;
        }
        eprintln!(
            "error: The following untracked working tree files would be overwritten by merge:"
        );
        for candidate in overwritten {
            eprintln!("\t{}", String::from_utf8_lossy(candidate));
        }
        eprintln!("Please move or remove them before you merge.");
        eprintln!("Aborting");
        return Err(GitError::Exit(2));
    }
    Ok(())
}

fn populated_gitlink_exists(worktree_root: &Path, path: &[u8]) -> Result<bool> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    Ok(sley_diff_merge::gitlink_git_dir(&worktree_root.join(rel)).is_some())
}

fn merge_worktree_path_exists(worktree_root: &Path, path: &[u8]) -> bool {
    let Ok(rel) = std::str::from_utf8(path) else {
        return false;
    };
    fs::symlink_metadata(worktree_root.join(rel)).is_ok()
}

fn merge_results_entry_map(results: &MergePathResults) -> MergeTreeMap {
    let mut entries = MergeTreeMap::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some(entry)) => {
                entries.insert(path.clone(), *entry);
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict { ours, .. } => {
                if let Some(entry) = ours {
                    entries.insert(path.clone(), *entry);
                }
            }
        }
    }
    entries
}

fn reset_index_and_worktree_to_commit_for_merge(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    commit: &ObjectId,
    recurse_submodules: bool,
) -> Result<()> {
    if recurse_submodules {
        commands::read_tree::reset_index_and_worktree_to_commit(
            worktree_root,
            git_dir,
            format,
            commit,
            true,
        )
    } else {
        sley_worktree::reset_index_and_worktree_to_commit_with_process_filter_metadata(
            worktree_root,
            git_dir,
            format,
            commit,
            Some(vec![("treeish".to_string(), commit.to_hex())]),
        )?;
        Ok(())
    }
}

// ===== pull / rebase / merge-continue =====
/// `git merge --abort` — implemented as git's `git reset --merge` (builtin/
/// merge.c invokes `cmd_reset` with `--merge`). HEAD did not move during a
/// `--no-commit` / conflicted merge, so this resets the index and worktree back
/// to *HEAD* (not ORIG_HEAD, which can be stale from an earlier completed
/// merge), restoring every path the merge staged or left conflicted while
/// preserving purely-local worktree modifications to untouched paths
/// (`oneway_merge` with `update=1`). Finally it clears the in-progress merge
/// bookkeeping.
pub(crate) fn cmd_merge_abort() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.is_file() {
        eprintln!("fatal: There is no merge to abort (MERGE_HEAD missing).");
        return Err(GitError::Exit(128));
    }

    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    reset_merge_to_head(&git_dir, &worktree_root, format)?;
    clear_in_progress_merge_state(&git_dir);
    apply_merge_autostash(&git_dir, format);
    Ok(())
}

/// `git reset --merge` against the current HEAD: rebuild the index from HEAD's
/// tree (stage 0), restore HEAD's worktree content for every path the
/// in-progress merge changed (a conflicted stage>0 entry, a stage-0 entry that
/// differs from HEAD, or a HEAD path the merge dropped), and leave all other
/// worktree paths — including purely-local modifications — untouched.
fn reset_merge_to_head(git_dir: &Path, worktree_root: &Path, format: ObjectFormat) -> Result<()> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let head_oid = resolve_revision(git_dir, format, "HEAD")?;
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let head_map = sley_diff_merge::flatten_tree(&db, format, &head_tree)?;
    let populated_head_gitlink_prefixes =
        populated_gitlink_directory_prefixes(worktree_root, &head_map)?;

    // The set of paths the merge touched relative to HEAD: anything in the
    // current index that is not a clean stage-0 match for HEAD's entry.
    let index = read_worktree_index(git_dir, format)?;
    let mut touched: BTreeSet<Vec<u8>> = BTreeSet::new();
    for entry in &index.entries {
        let path = entry.path.to_vec();
        if path_is_inside_populated_gitlink(&path, &populated_head_gitlink_prefixes) {
            continue;
        }
        let stage = index_entry_stage(entry);
        if stage > 0 {
            touched.insert(path);
            continue;
        }
        match head_map.get(&path) {
            Some((mode, oid)) if *mode == entry.mode && *oid == entry.oid => {}
            _ => {
                touched.insert(path);
            }
        }
    }
    // HEAD paths the merge dropped from the index also need restoring.
    let index_paths: BTreeSet<Vec<u8>> = index.entries.iter().map(|e| e.path.to_vec()).collect();
    for path in head_map.keys() {
        if !index_paths.contains(path) {
            touched.insert(path.clone());
        }
    }

    // Restore HEAD's content for the touched paths only.
    for path in &touched {
        match head_map.get(path) {
            Some((mode, oid)) => {
                let content = merge_worktree_content(&db, *mode, oid)?;
                merge_write_worktree_file(worktree_root, path, &content, *mode)?;
            }
            None => merge_remove_worktree_file(worktree_root, path)?,
        }
    }

    // Rewrite the index as HEAD's tree (stage 0).
    let mut entries: Vec<_> = head_map
        .iter()
        .map(|(path, (mode, oid))| merge_index_entry(path, *mode, *oid, 0))
        .collect();
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
    Ok(())
}

pub(crate) fn cmd_merge_continue() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.is_file() {
        eprintln!("fatal: There is no merge in progress (MERGE_HEAD missing).");
        return Err(GitError::Exit(128));
    }

    let format = repository_object_format(&git_dir)?;
    let message = read_merge_message_from_file_stripping_comments(&git_dir)?;
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
    let message = commit_cleanup_message(message, CommitCleanupMode::Whitespace, "#", false);
    let encoding = commit_encoding_header_from_config(git_dir);
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
            encoding,
            signature: None,
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
    commands::rerere::record_resolved_after_commit(git_dir, format)?;
    clear_in_progress_merge_state(git_dir);
    apply_merge_autostash(git_dir, format);
    if !quiet {
        print_branch_commit_summary(&writer, git_dir, format, &commit_oid, &message)?;
    }
    Ok(())
}

fn rebase_merge_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("rebase-merge")
}

pub(crate) fn rebase_in_progress(git_dir: &Path) -> bool {
    rebase_merge_dir(git_dir).is_dir()
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

pub(crate) fn print_commit_shortstat_between_trees(
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
    let stat_entries = collect_diff_stat_entries(&entries, db, None, false)?;
    write_diff_shortstat_materialized(&mut stdout, &stat_entries)?;
    Ok(())
}

pub(crate) fn conclude_rebase_step_via_commit(
    git_dir: &Path,
    format: ObjectFormat,
    mut author: Vec<u8>,
    committer: Vec<u8>,
    message: Vec<u8>,
    quiet: bool,
    allow_empty: bool,
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
    if !allow_empty && tree == parent_tree {
        eprintln!("nothing to commit, working tree clean");
        return Err(GitError::Exit(1));
    }
    if let Some(script_author) = read_rebase_author_script_identity(git_dir)? {
        author = script_author;
    }
    let encoding = commit_encoding_header_from_config(git_dir);
    let mut writer = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![parent_oid],
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding,
            signature: None,
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
        print_branch_commit_summary(&db, git_dir, format, &commit_oid, &message)?;
        print_commit_shortstat_between_trees(&db, format, &parent_tree, &tree)?;
    }
    Ok(())
}

fn read_rebase_author_script_identity(git_dir: &Path) -> Result<Option<Vec<u8>>> {
    let path = rebase_merge_dir(git_dir).join("author-script");
    let Ok(text) = fs::read(path) else {
        return Ok(None);
    };
    let Some((name, email, date)) = sley_sequencer::rebase::parse_author_script_bytes(&text) else {
        return Ok(None);
    };
    Ok(Some(sley_sequencer::format_commit_identity_bytes(
        &name, &email, &date,
    )?))
}

fn clear_in_progress_merge_state(git_dir: &Path) {
    let _ = fs::remove_file(git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("MERGE_MODE"));
    let _ = fs::remove_file(git_dir.join("AUTO_MERGE"));
}

fn write_auto_merge_ref(git_dir: &Path, tree: &ObjectId) -> Result<()> {
    fs::write(git_dir.join("AUTO_MERGE"), format!("{tree}\n"))?;
    Ok(())
}

fn peel_merge_target_to_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: ObjectId,
) -> Result<ObjectId> {
    sley_rev::peel_to_commit(db, format, &oid)
}

fn create_merge_autostash(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
) -> Result<bool> {
    let status = crate::collect_short_status(worktree_root, git_dir, format)?;
    let dirty = status
        .iter()
        .any(|entry| entry.index != b'?' && (entry.index != b' ' || entry.worktree != b' '));
    if !dirty {
        return Ok(false);
    }
    let Some(oid) = commands::stash::create_stash_for_autostash()? else {
        eprintln!("fatal: Cannot autostash");
        return Err(GitError::Exit(128));
    };
    fs::write(git_dir.join("MERGE_AUTOSTASH"), format!("{oid}\n"))?;
    println!("Created autostash: {}", format_log_abbrev_oid(&oid));
    let head = resolve_revision(git_dir, format, "HEAD")?;
    sley_worktree::reset_index_and_worktree_to_commit(worktree_root, git_dir, format, &head)?;
    Ok(true)
}

fn write_merge_autostash_marker(git_dir: &Path) -> Result<()> {
    if git_dir.join("MERGE_AUTOSTASH").exists() {
        Ok(())
    } else {
        Err(GitError::InvalidFormat("missing MERGE_AUTOSTASH".into()))
    }
}

pub(crate) fn apply_merge_autostash(git_dir: &Path, format: ObjectFormat) {
    apply_or_save_merge_autostash(git_dir, format, true);
}

pub(crate) fn save_merge_autostash(git_dir: &Path, format: ObjectFormat) {
    apply_or_save_merge_autostash(git_dir, format, false);
}

fn save_squash_conflict_autostash(git_dir: &Path, format: ObjectFormat) {
    let path = git_dir.join("MERGE_AUTOSTASH");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let oid_text = text.trim().to_string();
    let _ = fs::remove_file(&path);
    let Ok(oid) = ObjectId::from_hex(format, &oid_text) else {
        return;
    };
    if commands::stash::store_stash_commit(&oid, "autostash").is_ok() {
        println!("When finished, apply stashed changes with `git stash pop`");
    } else {
        eprintln!("error: cannot store {oid_text}");
    }
}

fn apply_or_save_merge_autostash(git_dir: &Path, format: ObjectFormat, attempt_apply: bool) {
    let path = git_dir.join("MERGE_AUTOSTASH");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let oid_text = text.trim().to_string();
    let _ = fs::remove_file(&path);
    if oid_text.is_empty() {
        return;
    }
    let Ok(oid) = ObjectId::from_hex(format, &oid_text) else {
        return;
    };
    let applied =
        attempt_apply && commands::stash::apply_stash_commit_quietly(&oid).unwrap_or(false);
    if applied {
        eprintln!("Applied autostash.");
        return;
    }
    let stored = commands::stash::store_stash_commit(&oid, "autostash").is_ok();
    if !stored {
        eprintln!("error: cannot store {oid_text}");
    } else if attempt_apply {
        print_merge_autostash_conflict_advice();
    } else {
        eprintln!("Autostash exists; creating a new stash entry.");
        eprintln!("Your changes are safe in the stash.");
        eprintln!("You can run \"git stash pop\" or \"git stash drop\" at any time.");
    }
}

fn print_merge_autostash_conflict_advice() {
    eprintln!("Your local changes are stashed, however applying them");
    eprintln!("resulted in conflicts.  You can either resolve the conflicts");
    eprintln!("and then discard the stash with \"git stash drop\", or, if you");
    eprintln!("do not want to resolve them now, run \"git reset --hard\" and");
    eprintln!("apply the local changes later by running \"git stash pop\".");
}

/// git's `write_merge_state` MERGE_MODE leg: write `.git/MERGE_MODE` alongside
/// MERGE_HEAD/MERGE_MSG whenever an in-progress merge is recorded. The body is
/// `no-ff` when `--no-ff` forced the merge, else empty — git always creates the
/// file so `merge --quit` / `--continue` have a complete state to consume.
fn write_merge_mode(git_dir: &Path, options: &MergeOptions) -> Result<()> {
    let body = if options.no_ff() { "no-ff" } else { "" };
    fs::write(git_dir.join("MERGE_MODE"), body)?;
    Ok(())
}

fn write_merge_state(
    git_dir: &Path,
    other_oids: &[ObjectId],
    message: impl AsRef<[u8]>,
    options: &MergeOptions,
    orig_head: Option<&ObjectId>,
) -> Result<()> {
    let mut merge_head = String::new();
    for oid in other_oids {
        merge_head.push_str(&format!("{oid}\n"));
    }
    fs::write(git_dir.join("MERGE_HEAD"), merge_head)?;
    fs::write(git_dir.join("MERGE_MSG"), message)?;
    write_merge_mode(git_dir, options)?;
    if let Some(orig_head) = orig_head {
        fs::write(git_dir.join("ORIG_HEAD"), format!("{orig_head}\n"))?;
    }
    Ok(())
}

pub(crate) fn read_worktree_index(git_dir: &Path, format: ObjectFormat) -> Result<Index> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    Index::parse(&fs::read(index_path)?, format)
}

pub(crate) fn index_unmerged_paths(index: &Index) -> Vec<Vec<u8>> {
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
    read_merge_message_from_file_with_comment_mode(git_dir, false)
}

fn read_merge_message_from_file_stripping_comments(git_dir: &Path) -> Result<Vec<u8>> {
    read_merge_message_from_file_with_comment_mode(git_dir, true)
}

fn read_merge_message_from_file_with_comment_mode(
    git_dir: &Path,
    strip_comments: bool,
) -> Result<Vec<u8>> {
    let merge_msg_path = git_dir.join("MERGE_MSG");
    let raw = if merge_msg_path.is_file() {
        fs::read(merge_msg_path)?
    } else {
        b"Merge commit\n".to_vec()
    };
    Ok(tag_stripspace_message(&raw, strip_comments))
}

fn merge_commit_reflog_message(message: &[u8]) -> Vec<u8> {
    format!("commit (merge): {}", commit_subject(message)).into_bytes()
}

pub(crate) fn print_branch_commit_summary(
    db: &FileObjectDatabase,
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
    // git's print_commit_summary appends `\n Author: <%an <%ae>>` when the
    // author identity differs from the committer identity (sequencer.c).
    let object = db.read_object(commit_oid)?;
    let commit = Commit::parse_ref(format, &object.body)?;
    let author = crate::commit_author_identity(&commit.author);
    let committer = crate::commit_author_identity(&commit.committer);
    if author != committer {
        println!(" Author: {author}");
    }
    Ok(())
}
