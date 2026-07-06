//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use sley::plumbing::{sley_refs, sley_remote, sley_rev, sley_worktree};
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use super::status::{StatusLineSink, status_long_tracking_lines};
use crate::*;

pub(crate) fn cmd_checkout(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut force = false;
    let mut recurse_submodules = None;
    let mut patch = false;
    let mut no_auto_advance = false;
    let mut unified_context = false;
    let mut inter_hunk_context = false;
    let mut guess = None::<bool>;
    let mut create_reflog = false;
    let mut path_merge = false;
    let mut conflict_implies_merge = false;
    let mut conflict_style = None::<sley_worktree::CheckoutConflictStyle>;
    let mut checkout_stage = None::<sley_worktree::CheckoutStage>;
    let mut branch_mode = CheckoutBranchMode::Existing;
    let mut ignore_other_worktrees = false;
    let mut track = None::<crate::commands::branch::BranchTrackMode>;
    let mut overlay_mode: Option<bool> = None;
    let mut overwrite_ignore = true;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut positional = Vec::new();
    let mut dashdash_index = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-m" | "--merge" => path_merge = true,
            "--no-merge" => {
                path_merge = false;
                conflict_implies_merge = false;
            }
            "--conflict" => {
                let Some(value) = iter.next() else {
                    return Err(GitError::Command(
                        "checkout --conflict requires a value".into(),
                    ));
                };
                conflict_style = Some(checkout_conflict_style(value)?);
                conflict_implies_merge = true;
            }
            value if value.starts_with("--conflict=") => {
                let value = &value["--conflict=".len()..];
                conflict_style = Some(checkout_conflict_style(value)?);
                conflict_implies_merge = true;
            }
            "--no-conflict" => {
                conflict_style = None;
                conflict_implies_merge = false;
            }
            "--ours" => checkout_stage = Some(sley_worktree::CheckoutStage::Ours),
            "--theirs" => checkout_stage = Some(sley_worktree::CheckoutStage::Theirs),
            "-p" | "--patch" => patch = true,
            "--no-patch" => patch = false,
            "--no-auto-advance" => no_auto_advance = true,
            "-U" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(true);
                };
                patch_validate_unified_context(value, true)?;
                unified_context = true;
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                patch_validate_unified_context(&value[2..], true)?;
                unified_context = true;
            }
            "--unified" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(false);
                };
                patch_validate_unified_context(value, false)?;
                unified_context = true;
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                patch_validate_unified_context(&value["--unified=".len()..], false)?;
                unified_context = true;
            }
            "--inter-hunk-context" => {
                let Some(value) = iter.next() else {
                    return commit_inter_hunk_context_requires_value_error();
                };
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = true;
            }
            "--inter-hunk-context=" => {
                return commit_inter_hunk_context_expects_numerical_value_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                patch_validate_inter_hunk_context(&value["--inter-hunk-context=".len()..])?;
                inter_hunk_context = true;
            }
            "--progress" | "--no-progress" => {}
            // `-l`/`--log` forces a reflog for the newly created branch.
            "-l" | "--log" => create_reflog = true,
            "--overlay" => overlay_mode = Some(true),
            "--no-overlay" => overlay_mode = Some(false),
            "--overwrite-ignore" => overwrite_ignore = true,
            "--no-overwrite-ignore" => overwrite_ignore = false,
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("checkout --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                let value = &value["--pathspec-from-file=".len()..];
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--ignore-other-worktrees" => ignore_other_worktrees = true,
            "--no-ignore-other-worktrees" => ignore_other_worktrees = false,
            "--recurse-submodules" => recurse_submodules = Some(true),
            "--no-recurse-submodules" => recurse_submodules = Some(false),
            "--guess" => guess = Some(true),
            "--no-guess" => guess = Some(false),
            "-t" | "--track" | "--track=direct" => {
                track = Some(crate::commands::branch::BranchTrackMode::Direct);
            }
            "--track=inherit" => {
                track = Some(crate::commands::branch::BranchTrackMode::Inherit);
            }
            "--no-track" => {
                track = Some(crate::commands::branch::BranchTrackMode::Never);
            }
            "-lb" => {
                if !matches!(branch_mode, CheckoutBranchMode::Existing) {
                    eprintln!("fatal: options '--detach' and '-b' cannot be used together");
                    return Err(GitError::Exit(128));
                }
                create_reflog = true;
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("checkout -b requires a branch".into()))?;
                branch_mode = CheckoutBranchMode::Create {
                    branch: branch.to_string(),
                    force: false,
                    orphan: false,
                };
            }
            "-b" => {
                if !matches!(branch_mode, CheckoutBranchMode::Existing) {
                    eprintln!("fatal: options '--detach' and '-b' cannot be used together");
                    return Err(GitError::Exit(128));
                }
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("checkout -b requires a branch".into()))?;
                branch_mode = CheckoutBranchMode::Create {
                    branch: branch.to_string(),
                    force: false,
                    orphan: false,
                };
            }
            "-B" => {
                if !matches!(branch_mode, CheckoutBranchMode::Existing) {
                    eprintln!("fatal: options '--detach' and '-B' cannot be used together");
                    return Err(GitError::Exit(128));
                }
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("checkout -B requires a branch".into()))?;
                branch_mode = CheckoutBranchMode::Create {
                    branch: branch.to_string(),
                    force: true,
                    orphan: false,
                };
            }
            "--detach" => {
                if !matches!(branch_mode, CheckoutBranchMode::Existing) {
                    eprintln!("fatal: options '-b' and '--detach' cannot be used together");
                    return Err(GitError::Exit(128));
                }
                branch_mode = CheckoutBranchMode::Detach;
            }
            "--orphan" => {
                let branch = iter.next().ok_or_else(|| {
                    GitError::Command("checkout --orphan requires a branch".into())
                })?;
                branch_mode = CheckoutBranchMode::Create {
                    branch: branch.to_string(),
                    force: false,
                    orphan: true,
                };
            }
            "--" => {
                dashdash_index = Some(positional.len());
                positional.extend(iter.map(|value| value.to_string()));
                break;
            }
            value => positional.push(value.to_string()),
        }
    }
    if no_auto_advance && !patch {
        eprintln!("fatal: the option '--no-auto-advance' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if unified_context && !patch {
        eprintln!("fatal: the option '--unified' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if inter_hunk_context && !patch {
        eprintln!("fatal: the option '--inter-hunk-context' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    // `--orphan` cannot set up branch tracking.
    if matches!(branch_mode, CheckoutBranchMode::Create { orphan: true, .. }) && track.is_some() {
        eprintln!("fatal: '--orphan' cannot be used with '-t'");
        return Err(GitError::Exit(128));
    }
    // `-p --overlay` is forbidden; only the implicit-overlay default pairs with -p.
    if patch && overlay_mode == Some(true) {
        eprintln!("fatal: options '-p' and '--overlay' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if pathspec_from_file.is_some() {
        // Git rejects pathspec args, then --detach, then --patch (in that order).
        let trailing_pathspec_args = match dashdash_index {
            Some(index) => positional.len() > index,
            None => positional.len() > 1,
        };
        if trailing_pathspec_args {
            eprintln!(
                "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
            );
            return Err(GitError::Exit(128));
        }
        if matches!(branch_mode, CheckoutBranchMode::Detach) {
            eprintln!(
                "fatal: options '--pathspec-from-file' and '--detach' cannot be used together"
            );
            return Err(GitError::Exit(128));
        }
        if patch {
            eprintln!(
                "fatal: options '--pathspec-from-file' and '--patch' cannot be used together"
            );
            return Err(GitError::Exit(128));
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let checkout_config = read_repo_config(&git_dir)?;
    if patch {
        return checkout_run_patch(
            &git_dir,
            format,
            &positional,
            dashdash_index,
            no_auto_advance,
        );
    }
    let recurse_submodules = recurse_submodules.unwrap_or_else(|| {
        checkout_config
            .get_bool("submodule", None, "recurse")
            .unwrap_or(false)
    });
    let guess = guess.unwrap_or_else(|| {
        checkout_config
            .get_bool("checkout", None, "guess")
            .unwrap_or(true)
    });
    // `git checkout -` is shorthand for `git checkout @{-1}`. Bare `@{-N}` is
    // interpreted as the Nth prior checkout target before branch/revision
    // resolution, so branch targets re-attach HEAD while detached targets still
    // flow through the normal revision path.
    if matches!(branch_mode, CheckoutBranchMode::Existing)
        && dashdash_index.is_none()
        && positional.len() == 1
    {
        let store = FileRefStore::new(&git_dir, format);
        if positional[0] == "-" {
            if let Some(name) = checkout_expand_previous_branch_arg(&git_dir, format, &store, 1)? {
                positional[0] = name;
            } else {
                positional[0] = "@{-1}".to_string();
            }
        } else if let Some(n) = checkout_previous_selector_n(&positional[0])
            && let Some(name) = checkout_expand_previous_branch_arg(&git_dir, format, &store, n)?
        {
            positional[0] = name;
        }
    }
    let checkout_old_head = resolve_ref_peeled(&FileRefStore::new(&git_dir, format), "HEAD")?
        .unwrap_or_else(|| ObjectId::null(format));
    let checkout_old_direct_head = checkout_direct_head(&FileRefStore::new(&git_dir, format))?;

    if matches!(
        track,
        Some(
            crate::commands::branch::BranchTrackMode::Direct
                | crate::commands::branch::BranchTrackMode::Inherit
        )
    ) && matches!(branch_mode, CheckoutBranchMode::Existing)
    {
        let [upstream] = positional.as_slice() else {
            return Err(GitError::Command(
                "checkout --track requires exactly one start point".into(),
            ));
        };
        let store = FileRefStore::new(&git_dir, format);
        let branch = checkout_track_branch_name(&store, upstream)?;
        branch_mode = CheckoutBranchMode::Create {
            branch,
            force: false,
            orphan: false,
        };
    }

    // Pathspec checkout: `checkout [<tree-ish>] [--] <pathspec>...` restores
    // paths (and, with a tree-ish, index entries) instead of switching HEAD.
    if matches!(branch_mode, CheckoutBranchMode::Existing) {
        let file_pathspecs: Vec<String>;
        let (source, paths): (Option<&str>, &[String]) = if let Some(pathspec_file) =
            pathspec_from_file.as_ref()
        {
            // `--pathspec-from-file`: the lone positional (if any) is the tree-ish
            // source; the pathspecs come from the file (option validation above
            // already rejected trailing pathspec arguments).
            let source = match positional.as_slice() {
                [] => None,
                [rev] => Some(rev.as_str()),
                _ => unreachable!("trailing pathspec args were rejected during validation"),
            };
            file_pathspecs = read_pathspecs_from_file(pathspec_file, pathspec_file_nul)?
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            (source, file_pathspecs.as_slice())
        } else {
            match dashdash_index {
                Some(index) => {
                    let (before, after) = positional.split_at(index);
                    match before {
                        [] => (None, after),
                        [rev] => (Some(rev.as_str()), after),
                        _ => {
                            return Err(GitError::Command(
                                "checkout with multiple tree-ish arguments is not supported".into(),
                            ));
                        }
                    }
                }
                None if positional.len() > 1 => {
                    // `checkout <rev> <paths>...` — but if the first arg is not a
                    // revision, every positional is a pathspec (git's
                    // disambiguation for `checkout <path> <path>...`).
                    if checkout_resolve_start_oid(&git_dir, format, &positional[0]).is_ok() {
                        (Some(positional[0].as_str()), &positional[1..])
                    } else {
                        (None, positional.as_slice())
                    }
                }
                None if positional.len() == 1 => {
                    // A single arg that is neither a branch nor a revision but
                    // names an existing file is a path checkout.
                    let value = &positional[0];
                    let store = FileRefStore::new(&git_dir, format);
                    let is_branch = branch_ref_name(value)
                        .ok()
                        .and_then(|name| {
                            sley_refs::resolve_ref_peeled(&store, &name).ok().flatten()
                        })
                        .is_some();
                    if !is_branch
                        && checkout_resolve_start_oid(&git_dir, format, value).is_err()
                        && (cwd.join(value).exists()
                            || checkout_index_has_path(
                                &git_dir,
                                &worktree_root,
                                &cwd,
                                format,
                                value,
                            )?)
                    {
                        if guess {
                            let _ = checkout_dwim_remote_branch(
                                &git_dir,
                                format,
                                &store,
                                &checkout_config,
                                value,
                                true,
                            )?;
                        }
                        (None, positional.as_slice())
                    } else {
                        (None, &[])
                    }
                }
                None => (None, &[]),
            }
        };
        if !paths.is_empty() {
            let resolved_paths: Vec<PathBuf> = paths
                .iter()
                .map(|path| {
                    let path = PathBuf::from(path);
                    if path.is_absolute() {
                        path
                    } else {
                        cwd.join(path)
                    }
                })
                .collect();
            match source {
                Some(rev) => {
                    if path_merge || conflict_implies_merge {
                        eprintln!(
                            "fatal: '--merge' cannot be used when checking out paths from a tree"
                        );
                        return Err(GitError::Exit(128));
                    }
                    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                    let oid = checkout_resolve_start_oid(&git_dir, format, rev)?;
                    let tree = sley_rev::peel_to_tree(&db, format, &oid)?;
                    sley_worktree::restore_index_and_worktree_paths_from_tree(
                        worktree_root,
                        git_dir,
                        format,
                        &tree,
                        &resolved_paths,
                        // git checkout defaults to overlay mode (leave paths
                        // absent from the tree-ish); --no-overlay removes them.
                        overlay_mode.unwrap_or(true),
                    )?;
                }
                None => {
                    let config = read_repo_config(&git_dir)?;
                    let conflict_style = conflict_style.unwrap_or_else(|| {
                        match checkout_config.get("merge", None, "conflictstyle") {
                            Some("diff3") | Some("zdiff3") => {
                                sley_worktree::CheckoutConflictStyle::Diff3
                            }
                            _ => sley_worktree::CheckoutConflictStyle::Merge,
                        }
                    });
                    let result = sley_worktree::checkout_index_paths(
                        worktree_root,
                        &git_dir,
                        format,
                        &resolved_paths,
                        sley_worktree::CheckoutIndexPathOptions {
                            force,
                            merge: path_merge || conflict_implies_merge,
                            overlay: overlay_mode.unwrap_or(true),
                            stage: checkout_stage,
                            conflict_style,
                            smudge_config: Some(&config),
                        },
                    )?;
                    if !quiet && dashdash_index.is_none() && result.restored > 0 {
                        let suffix = if result.restored == 1 { "" } else { "s" };
                        eprintln!("Updated {} path{} from the index", result.restored, suffix);
                    }
                }
            }
            commands::hooks::run_post_index_change_hook(false, false)?;
            run_post_checkout_hook(&checkout_old_head, &checkout_old_head, false)?;
            return Ok(());
        }
    }

    // `git checkout -f <commit-ish>` where the target is the commit HEAD already
    // points at (the common `git checkout -f HEAD` form, e.g. the trailing step
    // of upstream's test_commit_bulk): force-restore the index and working tree
    // to that commit without changing which branch HEAD is on, and stay silent on
    // success — exactly git's behavior when no branch switch happens.
    if force && matches!(branch_mode, CheckoutBranchMode::Existing) && positional.len() == 1 {
        let target = &positional[0];
        let store = FileRefStore::new(&git_dir, format);
        let head_commit = resolve_ref_peeled(&store, "HEAD")?;
        if let Ok(target_oid) = checkout_resolve_start_oid(&git_dir, format, target)
            && head_commit == Some(target_oid)
        {
            let switches_to_other_branch = branch_ref_name(target)
                .ok()
                .filter(|name| {
                    sley_refs::resolve_ref_peeled(&store, name)
                        .ok()
                        .flatten()
                        .is_some()
                })
                .is_some_and(|name| {
                    !matches!(
                        store.read_ref("HEAD"),
                        Ok(Some(RefTarget::Symbolic(current))) if current == name
                    )
                });
            if !switches_to_other_branch {
                if recurse_submodules {
                    commands::read_tree::reset_index_and_worktree_to_commit(
                        &worktree_root,
                        &git_dir,
                        format,
                        &target_oid,
                        true,
                    )?;
                } else {
                    sley_worktree::reset_index_and_worktree_to_commit(
                        &worktree_root,
                        &git_dir,
                        format,
                        &target_oid,
                    )?;
                }
                sley_sequencer::replay::remove_branch_state(&git_dir);
                return Ok(());
            }
        }
    }

    // `-f`: discard local index/worktree changes (including conflict stages)
    // before switching, so the clean-tree checkout below succeeds — git's
    // force semantics. Untracked files are preserved.
    if force {
        let store = FileRefStore::new(&git_dir, format);
        if let Ok(Some(head_oid)) = resolve_ref_peeled(&store, "HEAD") {
            if recurse_submodules {
                commands::read_tree::reset_index_and_worktree_to_commit(
                    &worktree_root,
                    &git_dir,
                    format,
                    &head_oid,
                    true,
                )?;
            } else {
                sley_worktree::reset_index_and_worktree_to_commit(
                    &worktree_root,
                    &git_dir,
                    format,
                    &head_oid,
                )?;
            }
        } else {
            // Unborn HEAD: discard the staged state entirely.
            let index_path = sley_worktree::repository_index_path(&git_dir);
            if index_path.exists() {
                let index = Index::parse(&fs::read(&index_path)?, format)?;
                for entry in &index.entries {
                    let full = worktree_root.join(entry.path.to_string());
                    if full.is_file() {
                        let _ = fs::remove_file(&full);
                    }
                }
                fs::write(
                    &index_path,
                    Index {
                        version: 2,
                        entries: Vec::new(),
                        extensions: Vec::new(),
                        checksum: None,
                    }
                    .write(format)?,
                )?;
            }
        }
    }

    let mut branch_update_rollback = None::<(String, Option<RefTarget>)>;
    let checkout_message = match branch_mode {
        CheckoutBranchMode::Detach => {
            if positional.len() > 1 {
                return Err(GitError::Command(
                    "checkout --detach accepts at most one commit".into(),
                ));
            }
            let target = positional.first().map(String::as_str).unwrap_or("HEAD");
            let target_oid = checkout_resolve_start_oid(&git_dir, format, target)?;
            let db = FileObjectDatabase::from_git_dir(&git_dir, format);
            let target_oid = sley_rev::peel_to_commit(&db, format, &target_oid)?;
            let store = FileRefStore::new(&git_dir, format);
            let from = checkout_reflog_from_name(&store);
            let config = read_repo_config(&git_dir)?;
            prefetch_local_promisor_checkout_blobs(&git_dir, format, &config, &target_oid)?;
            let old_head_direct = checkout_direct_head(&store)?;
            let subject = detached_checkout_subject(&git_dir, format, &target_oid);
            let message = format!("checkout: moving from {from} to {target}").into_bytes();
            if recurse_submodules {
                checkout_twoway_dirty(
                    &git_dir,
                    &worktree_root,
                    format,
                    Some(&target_oid),
                    recurse_submodules,
                    force,
                )?;
                detach_head_with_reflog(&git_dir, format, &target_oid, message)?;
            } else {
                match sley_worktree::checkout_detached_filtered(
                    &worktree_root,
                    &git_dir,
                    format,
                    &target_oid,
                    committer_identity_for_reflog()?,
                    message.clone(),
                    &config,
                ) {
                    Ok(_) => {}
                    Err(err) if checkout_is_dirty_tree_error(&err) => {
                        checkout_twoway_dirty(
                            &git_dir,
                            &worktree_root,
                            format,
                            Some(&target_oid),
                            recurse_submodules,
                            force,
                        )?;
                        detach_head_with_reflog(&git_dir, format, &target_oid, message)?;
                    }
                    Err(err) => return Err(err),
                }
            }
            commands::hooks::run_post_index_change_hook(true, false)?;
            sley_sequencer::replay::remove_branch_state(&git_dir);
            if !quiet {
                checkout_print_previous_detached_head(
                    &git_dir,
                    format,
                    &store,
                    &config,
                    old_head_direct,
                    &target_oid,
                )?;
                eprintln!(
                    "HEAD is now at {} {}",
                    checkout_format_abbrev_oid(&git_dir, format, &config, &target_oid)?,
                    subject
                );
            }
            run_post_checkout_hook(&checkout_old_head, &target_oid, true)?;
            commands::hooks::run_hook(
                "reference-transaction",
                commands::hooks::HookRun::default(),
            )?;
            checkout_show_local_changes(&git_dir, &target_oid, quiet, force)?;
            return Ok(());
        }
        CheckoutBranchMode::Existing => {
            let [branch] = positional.as_slice() else {
                if checkout_stage.is_some() {
                    eprintln!("fatal: '--ours/--theirs' needs the paths to check out");
                    return Err(GitError::Exit(128));
                }
                if positional.is_empty() {
                    // `git checkout` with no branch and no paths stays on the
                    // current branch: git reports local changes on stdout and
                    // succeeds (no error), leaving the index/worktree untouched.
                    let store = FileRefStore::new(&git_dir, format);
                    let Some(head) = resolve_ref_peeled(&store, "HEAD")? else {
                        eprintln!("fatal: You are on a branch yet to be born");
                        return Err(GitError::Exit(128));
                    };
                    let _ = checkout_show_local_changes(&git_dir, &head, quiet, force);
                    return Ok(());
                }
                return Err(GitError::Command(
                    "checkout currently supports: checkout [-q] <branch> or checkout [-q] -b|-B <branch> [<start>]".into(),
                ));
            };
            if checkout_stage.is_some() {
                eprintln!("fatal: '--ours/--theirs' cannot be used with switching branches");
                return Err(GitError::Exit(128));
            }
            // A target that is not an existing branch but resolves to a commit-ish
            // (e.g. `A^0`, a tag, a raw oid) is a *detached HEAD* checkout, not a
            // branch switch. git detaches HEAD at the resolved commit; treating it
            // as a branch name would mint a bogus `refs/heads/A^0` symref.
            let store = FileRefStore::new(&git_dir, format);
            let attached_current_branch = if matches!(branch.as_str(), "HEAD" | "@") {
                store.current_branch()?
            } else {
                None
            };
            let is_branch = branch_ref_name(branch)
                .ok()
                .and_then(|name| sley_refs::resolve_ref_peeled(&store, &name).ok().flatten())
                .is_some();
            if let Some(current_branch) = attached_current_branch {
                CheckoutMessage::Existing {
                    branch: current_branch,
                    announce: false,
                }
            } else if !is_branch
                && branch.contains("@{")
                && let Ok(Some(refname)) =
                    sley_rev::resolve_revision_symbolic_full_name(&git_dir, format, branch)
                && let Some(local_branch) = refname.strip_prefix("refs/heads/")
                && store.read_ref(&refname)?.is_some()
            {
                CheckoutMessage::Existing {
                    branch: local_branch.to_string(),
                    announce: true,
                }
            } else if !is_branch
                && let Ok(target_oid) = checkout_resolve_start_oid(&git_dir, format, branch)
            {
                // A tag (or other peelable object) detaches HEAD at the *commit*
                // it points to; `resolve_revision` yields the tag object itself,
                // so peel it before detaching (matches `git checkout <tag>`).
                let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                let target_oid = sley_rev::peel_to_commit(&db, format, &target_oid)?;
                let config = read_repo_config(&git_dir)?;
                prefetch_local_promisor_checkout_blobs(&git_dir, format, &config, &target_oid)?;
                let old_head_direct = checkout_direct_head(&store)?;
                let subject = detached_checkout_subject(&git_dir, format, &target_oid);
                let from = checkout_reflog_from_name(&store);
                let message = format!("checkout: moving from {from} to {branch}").into_bytes();
                if recurse_submodules {
                    checkout_twoway_dirty(
                        &git_dir,
                        &worktree_root,
                        format,
                        Some(&target_oid),
                        recurse_submodules,
                        force,
                    )?;
                    detach_head_with_reflog(&git_dir, format, &target_oid, message)?;
                } else {
                    match sley_worktree::checkout_detached_filtered(
                        &worktree_root,
                        &git_dir,
                        format,
                        &target_oid,
                        committer_identity_for_reflog()?,
                        message.clone(),
                        &config,
                    ) {
                        Ok(_) => {}
                        Err(err) if checkout_is_dirty_tree_error(&err) => {
                            checkout_twoway_dirty(
                                &git_dir,
                                &worktree_root,
                                format,
                                Some(&target_oid),
                                recurse_submodules,
                                force,
                            )?;
                            detach_head_with_reflog(&git_dir, format, &target_oid, message)?;
                        }
                        Err(err) => return Err(err),
                    }
                }
                sley_sequencer::replay::remove_branch_state(&git_dir);
                if !quiet {
                    if old_head_direct.is_none() {
                        checkout_print_detached_head_advice(&config, branch);
                    } else {
                        checkout_print_previous_detached_head(
                            &git_dir,
                            format,
                            &store,
                            &config,
                            old_head_direct,
                            &target_oid,
                        )?;
                    }
                    eprintln!(
                        "HEAD is now at {} {}",
                        checkout_format_abbrev_oid(&git_dir, format, &config, &target_oid)?,
                        subject
                    );
                }
                commands::hooks::run_post_index_change_hook(true, false)?;
                run_post_checkout_hook(&checkout_old_head, &target_oid, true)?;
                commands::hooks::run_hook(
                    "reference-transaction",
                    commands::hooks::HookRun::default(),
                )?;
                checkout_show_local_changes(&git_dir, &target_oid, quiet, force)?;
                return Ok(());
            } else if !is_branch {
                let branch_name = branch.clone();
                if guess
                    && let Some(dwim) = checkout_dwim_remote_branch(
                        &git_dir,
                        format,
                        &store,
                        &checkout_config,
                        &branch_name,
                        dashdash_index.is_none() && cwd.join(&branch_name).exists(),
                    )?
                {
                    let branch_ref = branch_ref_name(&branch_name)?;
                    branch_update_rollback =
                        Some((branch_ref.clone(), store.read_ref(&branch_ref)?));
                    let was_reset = checkout_create_or_reset_branch(
                        &git_dir,
                        &git_dir,
                        format,
                        &branch_name,
                        &dwim.remote_ref,
                        false,
                        create_reflog,
                        committer_identity_for_reflog()?,
                    )?;
                    let tracking_start = Some(dwim.remote_ref);
                    crate::commands::branch::branch_create_set_tracking(
                        &git_dir,
                        &store,
                        &branch_name,
                        tracking_start.as_ref(),
                        Some(crate::commands::branch::BranchTrackMode::Direct),
                        quiet,
                    )?;
                    if was_reset {
                        CheckoutMessage::Reset {
                            branch: branch_name,
                        }
                    } else {
                        CheckoutMessage::New {
                            branch: branch_name,
                        }
                    }
                } else {
                    eprintln!("error: pathspec '{branch}' did not match any file(s) known to git");
                    return Err(GitError::Exit(1));
                }
            } else {
                CheckoutMessage::Existing {
                    branch: branch.clone(),
                    announce: true,
                }
            }
        }
        CheckoutBranchMode::Create {
            branch,
            force,
            orphan,
        } => {
            let store = FileRefStore::new(&git_dir, format);
            let branch = checkout_expand_creation_branch_name(&git_dir, format, &store, branch)?;
            if orphan {
                if positional.len() > 1 {
                    eprintln!(
                        "fatal: Cannot update paths and switch to branch '{branch}' at the same time."
                    );
                    return Err(GitError::Exit(128));
                }
                // --orphan cannot reuse an existing branch name (there is no
                // force variant); reject before touching the index or HEAD.
                let branch_ref = branch_ref_name(&branch)?;
                if store.read_ref(&branch_ref)?.is_some() {
                    eprintln!("fatal: a branch named '{branch}' already exists");
                    return Err(GitError::Exit(128));
                }
                if let Some(start) = positional.first().map(String::as_str) {
                    let Some(start_oid) = resolve_checkout_start_oid(&git_dir, format, start)?
                    else {
                        eprintln!(
                            "fatal: '{start}' is not a commit and a branch '{branch}' cannot be created from it"
                        );
                        return Err(GitError::Exit(128));
                    };
                    // Switch the index + worktree to the start point through the
                    // shared two-way engine (git's merge_working_tree), so local
                    // modifications that would be overwritten abort the switch
                    // and leave HEAD on the current branch.
                    checkout_twoway_dirty(
                        &git_dir,
                        &worktree_root,
                        format,
                        Some(&start_oid),
                        recurse_submodules,
                        force,
                    )?;
                }
                checkout_switch_to_unborn_branch(&git_dir, &branch)?;
                if create_reflog {
                    store.write_reflog(&branch_ref, &[])?;
                }
                sley_sequencer::replay::remove_branch_state(&git_dir);
                if !quiet {
                    eprintln!("Switched to a new branch '{branch}'");
                }
                return Ok(());
            }
            if positional.len() > 1 {
                eprintln!(
                    "fatal: Cannot update paths and switch to branch '{branch}' at the same time."
                );
                return Err(GitError::Exit(128));
            }
            let start = positional.first().map(String::as_str).unwrap_or("HEAD");
            if matches!(
                track,
                Some(crate::commands::branch::BranchTrackMode::Direct)
            ) && !checkout_start_is_trackable_branch(&store, &checkout_config, start)?
            {
                eprintln!(
                    "fatal: cannot set up tracking information; starting point '{start}' is not a branch"
                );
                return Err(GitError::Exit(128));
            }
            if resolve_checkout_start_oid(&git_dir, format, start).is_err() {
                eprintln!(
                    "fatal: '{start}' is not a commit and a branch '{branch}' cannot be created from it"
                );
                return Err(GitError::Exit(128));
            }
            let branch_ref = branch_ref_name(&branch)?;
            branch_update_rollback = Some((branch_ref.clone(), store.read_ref(&branch_ref)?));
            let was_reset = checkout_create_or_reset_branch(
                &git_dir,
                &git_dir,
                format,
                &branch,
                start,
                force,
                create_reflog,
                committer_identity_for_reflog()?,
            )?;
            let tracking_start = positional.first().map(|start| {
                if start.contains("@{") {
                    sley_rev::resolve_revision_symbolic_full_name(&git_dir, format, start)
                        .ok()
                        .flatten()
                        .or_else(|| checkout_tracking_start_ref(&store, start))
                        .unwrap_or_else(|| start.clone())
                } else {
                    checkout_tracking_start_ref(&store, start).unwrap_or_else(|| start.clone())
                }
            });
            crate::commands::branch::branch_create_set_tracking(
                &git_dir,
                &store,
                &branch,
                tracking_start.as_ref(),
                track,
                quiet,
            )?;
            if was_reset {
                CheckoutMessage::Reset { branch }
            } else {
                CheckoutMessage::New { branch }
            }
        }
    };
    let branch = checkout_message.branch();

    let config = read_repo_config(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let branch_ref = branch_ref_name(branch)?;
    let checkout_reflog_from = checkout_reflog_from_name(&store);
    if !ignore_other_worktrees
        && !matches!(
            store.read_ref("HEAD"),
            Ok(Some(RefTarget::Symbolic(current))) if current == branch_ref
        )
        && let Some(worktree) = sley_worktree::find_shared_symref(&git_dir, "HEAD", &branch_ref)?
    {
        eprintln!(
            "fatal: '{}' is already used by worktree at '{}'",
            branch,
            worktree.path.display()
        );
        checkout_rollback_branch_update(&git_dir, format, &branch_update_rollback);
        return Err(GitError::Exit(128));
    }
    let branch_target = if store.read_ref(&branch_ref)?.is_some() {
        sley_refs::resolve_ref_peeled(&store, &branch_ref)?
    } else {
        None
    };
    if let Some(target) = branch_target {
        prefetch_local_promisor_checkout_blobs(&git_dir, format, &config, &target)?;
        let head_at_target = resolve_ref_peeled(&store, "HEAD")? == Some(target);
        if head_at_target && checkout_index_empty(&git_dir, format)? {
            sley_worktree::reset_index_and_worktree_to_commit(
                &worktree_root,
                &git_dir,
                format,
                &target,
            )?;
        } else if head_at_target && checkout_old_head != target {
            // `checkout -B <current-branch> <start>` force-reset the branch we are
            // already on. The ref moved before the worktree switch, so the helpers
            // below would compare the new HEAD against itself and skip the update.
            // Switch the worktree from the pre-reset HEAD tree to the new target
            // here (a two-way merge that carries local modifications, like git);
            // the downstream switch then no-ops on the worktree but still writes
            // the HEAD reflog and prints the message.
            let db = FileObjectDatabase::from_git_dir(&git_dir, format);
            let old_tree =
                commands::merge_rebase::commit_tree_oid(&db, format, &checkout_old_head)?;
            let target_tree = commands::merge_rebase::commit_tree_oid(&db, format, &target)?;
            if let Err(err) = commands::read_tree::checkout_two_way_engine(
                &git_dir,
                &worktree_root,
                format,
                &db,
                Some(&old_tree),
                &target_tree,
                commands::read_tree::UnpackPorcelain::Checkout,
                recurse_submodules,
                force,
            ) {
                checkout_rollback_branch_update(&git_dir, format, &branch_update_rollback);
                return Err(err);
            }
        }
    }
    if branch_target.is_none() {
        sley_sequencer::replay::remove_branch_state(&git_dir);
        if !quiet {
            checkout_message.print();
        }
        return Ok(());
    }
    if recurse_submodules || (branch_update_rollback.is_some() && !force) {
        let from = checkout_reflog_from.clone();
        let target = branch_target.ok_or_else(|| GitError::reference_not_found("branch"))?;
        if let Err(err) = checkout_twoway_dirty(
            &git_dir,
            &worktree_root,
            format,
            Some(&target),
            recurse_submodules,
            force,
        ) {
            checkout_rollback_branch_update(&git_dir, format, &branch_update_rollback);
            return Err(err);
        }
        if let Err(err) = switch_head_symbolic_with_reflog(&git_dir, format, branch, &target, &from)
        {
            checkout_rollback_branch_update(&git_dir, format, &branch_update_rollback);
            return Err(err);
        }
    } else {
        match sley_worktree::checkout_branch_filtered(
            &worktree_root,
            git_dir.clone(),
            format,
            branch,
            committer_identity_for_reflog()?,
            &config,
        ) {
            Ok(_) => {}
            Err(err) if checkout_is_dirty_tree_error(&err) => {
                let store = FileRefStore::new(&git_dir, format);
                let from = checkout_reflog_from.clone();
                let target = sley_refs::resolve_ref_peeled(&store, &branch_ref_name(branch)?)?
                    .ok_or_else(|| GitError::reference_not_found("branch"))?;
                if let Err(err) = checkout_twoway_dirty(
                    &git_dir,
                    &worktree_root,
                    format,
                    Some(&target),
                    recurse_submodules,
                    force,
                ) {
                    checkout_rollback_branch_update(&git_dir, format, &branch_update_rollback);
                    return Err(err);
                }
                if let Err(err) =
                    switch_head_symbolic_with_reflog(&git_dir, format, branch, &target, &from)
                {
                    checkout_rollback_branch_update(&git_dir, format, &branch_update_rollback);
                    return Err(err);
                }
            }
            Err(err) => {
                checkout_rollback_branch_update(&git_dir, format, &branch_update_rollback);
                return Err(err);
            }
        }
    }
    let branch_target = branch_target.ok_or_else(|| GitError::reference_not_found("branch"))?;
    ensure_head_checkout_reflog_entry(
        &FileRefStore::new(&git_dir, format),
        format,
        &branch_target,
        format!("checkout: moving from {checkout_reflog_from} to {branch}").into_bytes(),
    )?;
    sley_sequencer::replay::remove_branch_state(&git_dir);
    let checkout_new_head = resolve_ref_peeled(&FileRefStore::new(&git_dir, format), "HEAD")?
        .unwrap_or(checkout_old_head);
    commands::hooks::run_post_index_change_hook(true, false)?;
    run_post_checkout_hook(&checkout_old_head, &checkout_new_head, true)?;
    commands::hooks::run_hook("reference-transaction", commands::hooks::HookRun::default())?;
    if !quiet {
        checkout_print_previous_detached_head(
            &git_dir,
            format,
            &store,
            &config,
            checkout_old_direct_head,
            &checkout_new_head,
        )?;
        checkout_message.print();
    }
    if checkout_message.announces() {
        checkout_show_branch_tracking(&git_dir, format, branch, quiet)?;
    }
    // git's `show_local_changes`: report carried-forward worktree modifications
    // relative to the newly checked-out commit (`M\t<path>`, etc.).
    checkout_show_local_changes(&git_dir, &checkout_new_head, quiet, force)?;
    Ok(())
}

fn run_post_checkout_hook(
    old_head: &ObjectId,
    new_head: &ObjectId,
    branch_checkout: bool,
) -> Result<()> {
    let old = old_head.to_hex();
    let new = new_head.to_hex();
    commands::hooks::run_hook_l(
        "post-checkout",
        &[
            old.as_str(),
            new.as_str(),
            if branch_checkout { "1" } else { "0" },
        ],
    )?;
    Ok(())
}

fn checkout_index_has_path(
    git_dir: &Path,
    worktree_root: &Path,
    cwd: &Path,
    format: ObjectFormat,
    value: &str,
) -> Result<bool> {
    let absolute = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        cwd.join(value)
    };
    let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", absolute.display()))
    })?;
    let git_path = relative.to_string_lossy().replace('\\', "/").into_bytes();
    Ok(
        sley_worktree::read_repository_index(git_dir, format)?.is_some_and(|index| {
            index
                .entries
                .iter()
                .any(|entry| entry.path.as_bytes() == git_path)
        }),
    )
}

fn checkout_conflict_style(value: &str) -> Result<sley_worktree::CheckoutConflictStyle> {
    match value {
        "merge" => Ok(sley_worktree::CheckoutConflictStyle::Merge),
        "diff3" | "zdiff3" => Ok(sley_worktree::CheckoutConflictStyle::Diff3),
        other => {
            eprintln!("error: unknown conflict style '{other}'");
            Err(GitError::Exit(129))
        }
    }
}

fn checkout_print_detached_head_advice(config: &GitConfig, target: &str) {
    if !config
        .get_bool("advice", None, "detachedHead")
        .unwrap_or(true)
    {
        return;
    }
    eprintln!("Note: switching to '{target}'.");
    eprintln!();
    eprintln!("You are in 'detached HEAD' state. You can look around, make experimental");
    eprintln!("changes and commit them, and you can discard any commits you make in this");
    eprintln!("state without impacting any branches by switching back to a branch.");
    eprintln!();
    eprintln!("If you want to create a new branch to retain commits you create, you may");
    eprintln!("do so (now or later) by using -c with the switch command. Example:");
    eprintln!();
    eprintln!("  git switch -c <new-branch-name>");
    eprintln!();
    eprintln!("Or undo this operation with:");
    eprintln!();
    eprintln!("  git switch -");
    eprintln!();
    eprintln!("Turn off this advice by setting config variable advice.detachedHead to false");
    eprintln!();
}

fn checkout_direct_head(store: &FileRefStore) -> Result<Option<ObjectId>> {
    Ok(match store.read_ref("HEAD")? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        _ => None,
    })
}

fn checkout_print_previous_detached_head(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    old_head: Option<ObjectId>,
    new_head: &ObjectId,
) -> Result<()> {
    let Some(old_head) = old_head else {
        return Ok(());
    };
    if checkout_warn_orphaned_detached_commits(git_dir, format, store, config, &old_head, new_head)?
    {
        return Ok(());
    }
    eprintln!(
        "Previous HEAD position was {} {}",
        checkout_format_abbrev_oid(git_dir, format, config, &old_head)?,
        detached_checkout_subject(git_dir, format, &old_head)
    );
    Ok(())
}

fn checkout_warn_orphaned_detached_commits(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    old_head: &ObjectId,
    new_head: &ObjectId,
) -> Result<bool> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let old_commits = match sley_rev::walk_commit_metadata(git_dir, format, &db, [*old_head], false)
    {
        Ok(commits) => commits,
        Err(_) => return Ok(false),
    };
    let mut protected_roots = Vec::new();
    protected_roots.push(*new_head);
    for reference in store.list_all_refs()? {
        if reference.name == "HEAD" {
            continue;
        }
        if let Some(oid) = sley_refs::resolve_ref_peeled(store, &reference.name)?
            && let Ok(commit_oid) = sley_rev::peel_to_commit(&db, format, &oid)
        {
            protected_roots.push(commit_oid);
        }
    }
    let protected = sley_rev::walk_commit_metadata(git_dir, format, &db, protected_roots, false)?
        .into_iter()
        .map(|commit| commit.oid)
        .collect::<HashSet<_>>();
    let orphaned = old_commits
        .into_iter()
        .filter(|commit| !protected.contains(&commit.oid))
        .collect::<Vec<_>>();
    if orphaned.is_empty() {
        return Ok(false);
    }

    let noun = if orphaned.len() == 1 {
        "1 commit"
    } else {
        return checkout_print_multi_orphan_warning(git_dir, format, config, &orphaned);
    };
    eprintln!("Warning: you are leaving {noun} behind, not connected to");
    eprintln!("any of your branches:");
    eprintln!();
    for commit in orphaned.iter().take(4) {
        eprintln!(
            "  {} {}",
            checkout_format_abbrev_oid(git_dir, format, config, &commit.oid)?,
            detached_checkout_subject(git_dir, format, &commit.oid)
        );
    }
    eprintln!();
    eprintln!("If you want to keep it by creating a new branch, this may be a good time");
    eprintln!("to do so with:");
    eprintln!();
    eprintln!(" git branch <new-branch-name> {}", old_head.to_hex());
    Ok(true)
}

fn checkout_print_multi_orphan_warning(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    orphaned: &[sley_rev::CommitMetadata],
) -> Result<bool> {
    eprintln!(
        "Warning: you are leaving {} commits behind, not connected to",
        orphaned.len()
    );
    eprintln!("any of your branches:");
    eprintln!();
    for commit in orphaned.iter().take(4) {
        eprintln!(
            "  {} {}",
            checkout_format_abbrev_oid(git_dir, format, config, &commit.oid)?,
            detached_checkout_subject(git_dir, format, &commit.oid)
        );
    }
    eprintln!();
    eprintln!("If you want to keep them by creating a new branch, this may be a good time");
    eprintln!("to do so with:");
    eprintln!();
    eprintln!(
        " git branch <new-branch-name> {}",
        orphaned
            .first()
            .map(|commit| commit.oid.to_hex())
            .unwrap_or_default()
    );
    Ok(true)
}

fn checkout_format_abbrev_oid(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    oid: &ObjectId,
) -> Result<String> {
    let hex = oid.to_hex();
    let width = repository_abbrev_from_config(git_dir, format, config)?.unwrap_or(hex.len());
    let mut abbreviated = hex[..width.min(hex.len())].to_string();
    if abbreviated.len() < hex.len()
        && env::var("GIT_PRINT_SHA1_ELLIPSIS").is_ok_and(|value| value.eq_ignore_ascii_case("yes"))
    {
        abbreviated.push_str("...");
    }
    Ok(abbreviated)
}

fn checkout_previous_selector_n(value: &str) -> Option<usize> {
    let inner = value
        .strip_prefix("@{-")
        .and_then(|value| value.strip_suffix('}'))?;
    inner.parse::<usize>().ok().filter(|n| *n > 0)
}

fn checkout_expand_previous_branch_arg(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    n: usize,
) -> Result<Option<String>> {
    let Some(name) = sley_rev::nth_prior_checkout_branch_name(git_dir, format, n)? else {
        return Ok(None);
    };
    let branch = name.strip_prefix("refs/heads/").unwrap_or(&name);
    let Ok(refname) = branch_ref_name(branch) else {
        return Ok(None);
    };
    if store.read_ref(&refname)?.is_some() {
        Ok(Some(branch.to_string()))
    } else {
        Ok(None)
    }
}

fn checkout_resolve_start_oid(
    git_dir: &Path,
    format: ObjectFormat,
    start: &str,
) -> Result<ObjectId> {
    resolve_checkout_start_oid(git_dir, format, start)?
        .ok_or_else(|| GitError::not_found(format!("revision {start}")))
}

fn checkout_expand_creation_branch_name(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch: String,
) -> Result<String> {
    if let Some(inner) = branch
        .strip_prefix("@{-")
        .and_then(|value| value.strip_suffix('}'))
    {
        let n = inner
            .parse::<usize>()
            .map_err(|_| GitError::InvalidFormat(format!("invalid branch name: '{branch}'")))?;
        return Ok(sley_rev::nth_prior_checkout_branch_name(git_dir, format, n)?.unwrap_or(branch));
    }
    if branch.contains("@{")
        && let Ok(Some(refname)) =
            sley_rev::resolve_revision_symbolic_full_name(git_dir, format, &branch)
        && let Some(local) = refname.strip_prefix("refs/heads/")
        && store.read_ref(&refname)?.is_some()
    {
        return Ok(local.to_string());
    }
    Ok(branch)
}

fn checkout_track_branch_name(store: &FileRefStore, upstream: &str) -> Result<String> {
    if let Some(rest) = upstream.strip_prefix("refs/remotes/")
        && let Some((_, branch)) = rest.split_once('/')
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
    }
    if let Some(rest) = upstream.strip_prefix("remotes/")
        && let Some((_, branch)) = rest.split_once('/')
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
    }
    if let Some(branch) = upstream.strip_prefix("refs/heads/")
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
    }
    let remote_ref = format!("refs/remotes/{upstream}");
    if store.read_ref(&remote_ref)?.is_some()
        && let Some((_, branch)) = upstream.split_once('/')
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
    }
    Ok(upstream.rsplit('/').next().unwrap_or(upstream).to_string())
}

fn checkout_tracking_start_ref(store: &FileRefStore, start: &str) -> Option<String> {
    let candidates = if start == "HEAD" {
        vec!["HEAD".to_string()]
    } else if start.starts_with("refs/") {
        vec![start.to_string()]
    } else if let Some(rest) = start.strip_prefix("remotes/") {
        vec![format!("refs/remotes/{rest}")]
    } else {
        vec![
            format!("refs/{start}"),
            format!("refs/tags/{start}"),
            format!("refs/heads/{start}"),
            format!("refs/remotes/{start}"),
            format!("refs/remotes/{start}/HEAD"),
        ]
    };
    candidates.into_iter().find_map(|candidate| {
        checkout_tracking_direct_ref(store, &candidate)
            .or_else(|| store.read_ref(&candidate).ok().flatten().map(|_| candidate))
    })
}

fn checkout_tracking_direct_ref(store: &FileRefStore, name: &str) -> Option<String> {
    let mut current = name.to_string();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current.clone()) {
            return None;
        }
        match store.read_ref(&current).ok().flatten()? {
            RefTarget::Direct(_) => return Some(current),
            RefTarget::Symbolic(next) => current = next,
        }
    }
}

fn checkout_start_is_trackable_branch(
    store: &FileRefStore,
    config: &GitConfig,
    start: &str,
) -> Result<bool> {
    if start == "HEAD" {
        return Ok(store.current_branch()?.is_some());
    }
    if !start.contains('/') && store.read_ref(&format!("refs/tags/{start}"))?.is_some() {
        return Ok(false);
    }
    if let Ok(local_ref) = branch_ref_name(start)
        && store.read_ref(&local_ref)?.is_some()
    {
        return Ok(true);
    }
    if start.starts_with("refs/heads/") || start.starts_with("refs/remotes/") {
        return Ok(store.read_ref(start)?.is_some());
    }
    if let Some(rest) = start.strip_prefix("remotes/") {
        return Ok(store.read_ref(&format!("refs/remotes/{rest}"))?.is_some());
    }
    for remote in checkout_config_remote_names(config) {
        let Some(branch) = start.strip_prefix(&format!("{remote}/")) else {
            continue;
        };
        let remote_ref = format!("refs/remotes/{remote}/{branch}");
        if store.read_ref(&remote_ref)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

struct CheckoutDwimRemoteBranch {
    remote_ref: String,
}

fn checkout_dwim_remote_branch(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    name: &str,
    could_be_checkout_path: bool,
) -> Result<Option<CheckoutDwimRemoteBranch>> {
    let mut matches = checkout_dwim_remote_candidates(store, config, name)?;
    if matches.is_empty() {
        return Ok(None);
    }

    if let Some(default_remote) = config.get("checkout", None, "defaultRemote") {
        let default_ref = format!("refs/remotes/{default_remote}/{name}");
        if matches.iter().any(|candidate| candidate == &default_ref) {
            matches = vec![default_ref];
        }
    }

    if matches.len() == 1 {
        if could_be_checkout_path {
            eprintln!(
                "fatal: '{name}' could be both a local file and a tracking branch.\nPlease use -- (and optionally --no-guess) to disambiguate"
            );
            return Err(GitError::Exit(128));
        }
        let remote_ref = matches.remove(0);
        let oid = sley_refs::resolve_ref_peeled(store, &remote_ref)?
            .ok_or_else(|| GitError::reference_not_found(&remote_ref))?;
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        let _ = sley_rev::peel_to_commit(&db, format, &oid)?;
        return Ok(Some(CheckoutDwimRemoteBranch { remote_ref }));
    }

    if config
        .get_bool("advice", None, "checkoutAmbiguousRemoteBranchName")
        .unwrap_or(true)
    {
        eprintln!(
            "hint: If you meant to check out a remote tracking branch on, e.g. 'origin',\nhint: you can do so by fully qualifying the name with the --track option:\nhint: \nhint:     git checkout --track origin/<name>\nhint: \nhint: If you'd like to always have checkouts of an ambiguous <name> prefer\nhint: one remote, e.g. the 'origin' remote, consider setting\nhint: checkout.defaultRemote=origin in your config."
        );
    }
    eprintln!(
        "fatal: '{name}' matched multiple ({}) remote tracking branches",
        matches.len()
    );
    Err(GitError::Exit(128))
}

fn checkout_dwim_remote_candidates(
    store: &FileRefStore,
    config: &GitConfig,
    name: &str,
) -> Result<Vec<String>> {
    let src_name = format!("refs/heads/{name}");
    let mut matches = Vec::new();
    for remote in checkout_config_remote_names(config) {
        for fetch in config
            .get_all("remote", Some(&remote), "fetch")
            .into_iter()
            .flatten()
        {
            let Ok(refspec) = parse_refspec(fetch) else {
                continue;
            };
            if refspec.negative {
                continue;
            }
            let Some(src) = refspec.src.as_deref() else {
                continue;
            };
            let Some(dst) = refspec.dst.as_deref() else {
                continue;
            };
            let remote_ref = if refspec.pattern {
                let Some((src_prefix, src_suffix)) = src.split_once('*') else {
                    continue;
                };
                let Some(middle) = src_name
                    .strip_prefix(src_prefix)
                    .and_then(|value| value.strip_suffix(src_suffix))
                else {
                    continue;
                };
                let Some((dst_prefix, dst_suffix)) = dst.split_once('*') else {
                    continue;
                };
                format!("{dst_prefix}{middle}{dst_suffix}")
            } else if src == src_name {
                dst.to_string()
            } else {
                continue;
            };
            if store.read_ref(&remote_ref)?.is_some() && !matches.contains(&remote_ref) {
                matches.push(remote_ref);
            }
        }
    }
    Ok(matches)
}

fn checkout_config_remote_names(config: &GitConfig) -> Vec<String> {
    let mut remotes = Vec::new();
    for section in &config.sections {
        if section.name == "remote"
            && let Some(remote) = section.subsection.as_ref()
            && !remotes.contains(remote)
        {
            remotes.push(remote.clone());
        }
    }
    remotes
}

pub(crate) fn cmd_switch(args: &[String]) -> Result<()> {
    // `switch --orphan <name>` clears the index and worktree of files carried
    // from the old HEAD (a twoway checkout to the empty tree), unlike
    // `checkout --orphan` which keeps them staged.
    if let Some(pos) = args.iter().position(|arg| arg == "--orphan") {
        let Some(branch) = args.get(pos + 1) else {
            return Err(GitError::Command(
                "switch --orphan requires a branch".into(),
            ));
        };
        let cwd = env::current_dir()?;
        let git_dir = crate::session::cli_git_dir_from(&cwd)?;
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        let format = repository_object_format(&git_dir)?;
        checkout_twoway_dirty(&git_dir, &worktree_root, format, None, false, false)?;
        checkout_switch_to_unborn_branch(&git_dir, branch)?;
        sley_sequencer::replay::remove_branch_state(&git_dir);
        if !args.iter().any(|arg| arg == "-q" || arg == "--quiet") {
            eprintln!("Switched to a new branch '{branch}'");
        }
        return Ok(());
    }
    let mut checkout_args = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-c" | "--create" => {
                checkout_args.push("-b".to_string());
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("switch -c requires a branch".into()))?;
                checkout_args.push(branch.to_string());
            }
            value if value.starts_with("--create=") => {
                checkout_args.push("-b".to_string());
                checkout_args.push(
                    value
                        .strip_prefix("--create=")
                        .ok_or_else(|| {
                            GitError::Command("switch --create requires a branch".into())
                        })?
                        .to_string(),
                );
            }
            "-d" => checkout_args.push("--detach".to_string()),
            "-C" | "--force-create" => {
                checkout_args.push("-B".to_string());
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("switch -C requires a branch".into()))?;
                checkout_args.push(branch.to_string());
            }
            value if value.starts_with("--force-create=") => {
                checkout_args.push("-B".to_string());
                checkout_args.push(
                    value
                        .strip_prefix("--force-create=")
                        .ok_or_else(|| {
                            GitError::Command("switch --force-create requires a branch".into())
                        })?
                        .to_string(),
                );
            }
            value => checkout_args.push(value.to_string()),
        }
    }
    cmd_checkout(&checkout_args)
}

pub(crate) fn cmd_restore(args: &[String]) -> Result<()> {
    let mut paths = Vec::new();
    let mut parsing_options = true;
    let mut staged = false;
    let mut worktree = false;
    let mut _quiet = false;
    let mut ignore_unmerged = false;
    let mut path_merge = false;
    let mut conflict_implies_merge = false;
    let mut conflict_style = None::<sley_worktree::CheckoutConflictStyle>;
    let mut checkout_stage = None::<sley_worktree::CheckoutStage>;
    let mut patch = false;
    let mut unified_context = false;
    let mut inter_hunk_context = false;
    let mut source = None::<String>;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            if pathspec_from_file.is_some() {
                eprintln!(
                    "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                );
                return Err(GitError::Exit(128));
            }
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "--worktree" | "-W" => worktree = true,
            "--staged" | "-S" => staged = true,
            "-q" | "--quiet" => _quiet = true,
            "--no-quiet" => _quiet = false,
            "-m" | "--merge" => path_merge = true,
            "--no-merge" => {
                path_merge = false;
                conflict_implies_merge = false;
            }
            "--conflict" => {
                let Some(value) = iter.next() else {
                    return Err(GitError::Command(
                        "restore --conflict requires a value".into(),
                    ));
                };
                conflict_style = Some(checkout_conflict_style(value)?);
                conflict_implies_merge = true;
            }
            value if value.starts_with("--conflict=") => {
                let value = &value["--conflict=".len()..];
                conflict_style = Some(checkout_conflict_style(value)?);
                conflict_implies_merge = true;
            }
            "--no-conflict" => {
                conflict_style = None;
                conflict_implies_merge = false;
            }
            "--ours" => checkout_stage = Some(sley_worktree::CheckoutStage::Ours),
            "--theirs" => checkout_stage = Some(sley_worktree::CheckoutStage::Theirs),
            "-p" | "--patch" => patch = true,
            "--no-patch" => patch = false,
            "-U" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(true);
                };
                patch_validate_unified_context(value, true)?;
                unified_context = true;
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                patch_validate_unified_context(&value[2..], true)?;
                unified_context = true;
            }
            "--unified" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(false);
                };
                patch_validate_unified_context(value, false)?;
                unified_context = true;
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                patch_validate_unified_context(&value["--unified=".len()..], false)?;
                unified_context = true;
            }
            "--inter-hunk-context" => {
                let Some(value) = iter.next() else {
                    return commit_inter_hunk_context_requires_value_error();
                };
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = true;
            }
            "--inter-hunk-context=" => {
                return commit_inter_hunk_context_expects_numerical_value_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                patch_validate_inter_hunk_context(&value["--inter-hunk-context=".len()..])?;
                inter_hunk_context = true;
            }
            "--progress"
            | "--no-progress"
            | "--overlay"
            | "--no-overlay"
            | "--ignore-skip-worktree-bits"
            | "--no-ignore-skip-worktree-bits"
            | "--no-recurse-submodules" => {}
            "--ignore-unmerged" => ignore_unmerged = true,
            "--no-ignore-unmerged" => ignore_unmerged = false,
            "--source" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("restore --source requires a value".into()))?;
                source = Some(value.clone());
            }
            "-s" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("restore -s requires a value".into()))?;
                source = Some(value.clone());
            }
            value if value.starts_with("--source=") => {
                let value = value
                    .strip_prefix("--source=")
                    .ok_or_else(|| GitError::Command("restore --source requires a value".into()))?;
                source = Some(value.to_string());
            }
            value if value.starts_with("-s") && value.len() > 2 => {
                source = Some(value[2..].to_string());
            }
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("restore --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("restore --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|option| option == b'S' || option == b'W') =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b'S' => staged = true,
                        b'W' => worktree = true,
                        _ => unreachable!("restore short-option group was filtered"),
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported restore option {value}"
                )));
            }
            value => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                paths.push(PathBuf::from(value));
            }
        }
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if unified_context && !patch {
        eprintln!("fatal: the option '--unified' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if inter_hunk_context && !patch {
        eprintln!("fatal: the option '--inter-hunk-context' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if ignore_unmerged && patch {
        eprintln!("fatal: '--ignore-unmerged' cannot be used with updating paths");
        return Err(GitError::Exit(128));
    }
    if ignore_unmerged && path_merge {
        eprintln!("fatal: options '--ignore-unmerged' and '-m' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if staged {
        if checkout_stage.is_some() {
            eprintln!("fatal: '--ours' or '--theirs' cannot be used with --staged");
            return Err(GitError::Exit(128));
        }
        if path_merge || conflict_implies_merge {
            eprintln!("fatal: '--merge' or '--conflict' cannot be used with --staged");
            return Err(GitError::Exit(128));
        }
    }
    if source.is_some() && (path_merge || conflict_implies_merge || checkout_stage.is_some()) {
        eprintln!(
            "fatal: '--merge', '--ours', or '--theirs' cannot be used when checking out of a tree"
        );
        return Err(GitError::Exit(128));
    }
    if pathspec_from_file.is_some() && patch {
        eprintln!("fatal: options '--pathspec-from-file' and '--patch' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if patch {
        let cwd = env::current_dir()?;
        let git_dir = crate::session::cli_git_dir_from(&cwd)?;
        let format = repository_object_format(&git_dir)?;
        return restore_run_patch(
            &git_dir,
            format,
            &paths,
            source.as_deref(),
            staged,
            worktree,
        );
    }
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    if paths.is_empty() {
        eprintln!("fatal: you must specify path(s) to restore");
        return Err(GitError::Exit(128));
    }
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let resolved_paths = paths
        .into_iter()
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .collect::<Vec<_>>();
    let source_tree = if let Some(source) = source.as_deref() {
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let oid = resolve_revision(&git_dir, format, source)?;
        Some(sley_rev::peel_to_tree(&db, format, &oid)?)
    } else {
        None
    };
    if staged && worktree {
        // git restore defaults to no-overlay: paths absent from the source are
        // removed from the index and working tree.
        if let Some(tree_oid) = source_tree.as_ref() {
            sley_worktree::restore_index_and_worktree_paths_from_tree(
                worktree_root,
                git_dir,
                format,
                tree_oid,
                &resolved_paths,
                false,
            )?;
        } else {
            sley_worktree::restore_index_and_worktree_paths_from_head(
                worktree_root,
                git_dir,
                format,
                &resolved_paths,
                false,
            )?;
        }
    } else if staged {
        if let Some(tree_oid) = source_tree.as_ref() {
            sley_worktree::restore_index_paths_from_tree(
                worktree_root,
                git_dir,
                format,
                tree_oid,
                &resolved_paths,
            )?;
        } else {
            sley_worktree::restore_index_paths_from_head(
                worktree_root,
                git_dir,
                format,
                &resolved_paths,
            )?;
        }
    } else if let Some(tree_oid) = source_tree.as_ref() {
        sley_worktree::restore_worktree_paths_from_tree(
            worktree_root,
            git_dir,
            format,
            tree_oid,
            &resolved_paths,
        )?;
    } else {
        let config = read_repo_config(&git_dir)?;
        let conflict_style =
            conflict_style.unwrap_or_else(|| match config.get("merge", None, "conflictstyle") {
                Some("diff3") | Some("zdiff3") => sley_worktree::CheckoutConflictStyle::Diff3,
                _ => sley_worktree::CheckoutConflictStyle::Merge,
            });
        sley_worktree::checkout_index_paths(
            worktree_root,
            &git_dir,
            format,
            &resolved_paths,
            sley_worktree::CheckoutIndexPathOptions {
                force: ignore_unmerged,
                merge: path_merge || conflict_implies_merge,
                overlay: false,
                stage: checkout_stage,
                conflict_style,
                smudge_config: Some(&config),
            },
        )?;
    }
    Ok(())
}

/// Two-way checkout through the shared `sley_unpack_trees::twoway_merge`
/// engine (git's `merge_working_tree` in `builtin/checkout.c`): switch the
/// index + working tree from the current HEAD's tree to `target`'s tree,
/// carrying forward local modifications where the merge is safe and aborting
/// with git's exact "would be overwritten by checkout" porcelain otherwise.
///
/// This is the SINGLE two-way path behind `git checkout <branch>`,
/// `git switch`, and `git checkout --detach`. It replaces the former bespoke
/// path-by-path two-way reimplementation: the engine owns `verify_uptodate` /
/// `verify_absent` / staged-deletion semantics, so the whole checkout class
/// inherits git's behaviour from one primitive rather than a parallel copy.
///
/// `target` of `None` switches to the empty tree (`switch --orphan`).
/// git's `show_local_changes` (`builtin/checkout.c`): after a successful branch
/// switch, print the `--name-status` diff of the *worktree* against the newly
/// checked-out commit so a carried-forward local modification is reported (e.g.
/// `M\tsame`). This is `git diff-index --name-status <new_commit>` against the
/// (just-rebuilt) index + worktree, run for output only.
///
/// git gates this on `!opts->discard_changes && !opts->quiet &&
/// new_branch_info->commit`: a force checkout (`-f`, which discards local
/// changes) and a quiet checkout (`-q`) print nothing, and there is nothing to
/// diff against when the target has no commit (an unborn branch).
fn checkout_show_local_changes(
    git_dir: &Path,
    new_commit: &ObjectId,
    quiet: bool,
    force: bool,
) -> Result<()> {
    if quiet || force {
        return Ok(());
    }
    // git's `new_branch_info->commit` guard: there is nothing to diff against
    // when switching to an unborn branch (HEAD has no commit), so the zero OID
    // suppresses the local-changes report. Without this a `checkout -B <name>`
    // on a fresh `init` would run `diff-index 0000…0000` and fail.
    if new_commit.is_null() {
        return Ok(());
    }
    if checkout_sparse_checkout_enabled(git_dir) {
        return Ok(());
    }
    // Reuse the shared `diff-index` renderer (byte-identical with git's
    // name-status output). It diffs the tree-ish against the working tree by
    // default — exactly git's `run_diff_index(&rev, 0)`.
    commands::diff_index::cmd_diff_index(&["--name-status".to_string(), new_commit.to_hex()])
}

fn checkout_sparse_checkout_enabled(git_dir: &Path) -> bool {
    GitConfig::read(git_dir.join("config.worktree"))
        .ok()
        .and_then(|config| config.get_bool("core", None, "sparseCheckout"))
        == Some(true)
        || GitConfig::read(git_dir.join("config"))
            .ok()
            .and_then(|config| config.get_bool("core", None, "sparseCheckout"))
            == Some(true)
}

fn prefetch_local_promisor_checkout_blobs(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    commit_oid: &ObjectId,
) -> Result<bool> {
    let remotes = crate::promisor_remote_names(config);
    if remotes.is_empty() {
        return Ok(false);
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut wants = Vec::new();
    collect_missing_checkout_blob_wants(&db, format, *commit_oid, &mut wants)?;
    if wants.is_empty() {
        return Ok(true);
    }
    // Try every configured promisor remote in turn (git's
    // `promisor_remote_get_direct`): a remote that cannot supply the blobs
    // errors during negotiation, so fall through to the next.
    sley_protocol::set_packet_trace_identity("fetch");
    for remote_name in &remotes {
        let Some(url) = config.get("remote", Some(remote_name), "url") else {
            continue;
        };
        let Ok(remote_git_dir) = commands::remote::ls_remote_git_dir(url) else {
            continue;
        };
        let _ = sley_remote::install_fetch_pack_via_local_upload_pack(
            git_dir,
            &remote_git_dir,
            format,
            wants.clone(),
            None,
            true,
            false,
            None,
            false,
            None,
        );
    }
    sley_protocol::set_packet_trace_identity("checkout");
    // Anything the promisor remotes could not supply stays missing: git's
    // `promisor_remote_get_direct` dies once every remote has been tried.
    db.refresh_read_cache();
    if let Some(missing) = wants.iter().find(|oid| !db.contains(oid).unwrap_or(false)) {
        eprintln!("fatal: could not fetch {missing} from promisor remote");
        return Err(GitError::Exit(128));
    }
    Ok(true)
}

fn collect_missing_checkout_blob_wants(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: ObjectId,
    wants: &mut Vec<ObjectId>,
) -> Result<()> {
    let object = db.read_object(&commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {commit_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    collect_missing_tree_blob_wants(db, format, commit.tree, wants)
}

fn collect_missing_tree_blob_wants(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: ObjectId,
    wants: &mut Vec<ObjectId>,
) -> Result<()> {
    let object = db.read_object(&tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    for entry in Tree::parse(format, &object.body)?.entries {
        if entry.is_tree() {
            collect_missing_tree_blob_wants(db, format, entry.oid, wants)?;
        } else if !entry.is_gitlink() && !db.contains(&entry.oid)? {
            wants.push(entry.oid);
        }
    }
    Ok(())
}

fn checkout_index_empty(git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    Ok(read_repository_index(git_dir, format)?
        .map(|index| index.entries.is_empty())
        .unwrap_or(true))
}

fn checkout_show_branch_tracking(
    git_dir: &Path,
    format: ObjectFormat,
    branch: &str,
    quiet: bool,
) -> Result<()> {
    if quiet {
        return Ok(());
    }
    let store = FileRefStore::new(git_dir, format);
    let branch_ref = branch_ref_name(branch)?;
    let Some(RefTarget::Direct(oid)) = store.read_ref(&branch_ref)? else {
        return Ok(());
    };
    let mut sink = StatusLineSink::new(true, None);
    status_long_tracking_lines(
        git_dir,
        format,
        &store,
        &branch_ref,
        &oid,
        true,
        false,
        &mut sink,
    )?;
    let mut buf = Vec::new();
    sink.write_to(&mut buf);
    if buf.ends_with(b"\n\n") {
        buf.pop();
    }
    io::stdout().lock().write_all(&buf)?;
    Ok(())
}

fn checkout_twoway_dirty(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    target: Option<&ObjectId>,
    recurse_submodules: bool,
    overwrite_untracked: bool,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);

    // The tree being checked out. `None` (orphan switch) maps to the empty tree
    // so every currently-tracked path is removed.
    let target_tree = match target {
        Some(target) => commands::merge_rebase::commit_tree_oid(&db, format, target)?,
        None => ObjectId::empty_tree(format),
    };

    // The tree of the HEAD being left (git's `old_branch_info->commit`). `None`
    // when HEAD is unborn — the engine then sees an empty `oldtree` side.
    let refs = FileRefStore::new(git_dir, format);
    let old_tree = match commands::merge_rebase::head_commit_oid(&refs)? {
        Some(head) => Some(commands::merge_rebase::commit_tree_oid(&db, format, &head)?),
        None => None,
    };

    commands::read_tree::checkout_two_way_engine(
        git_dir,
        worktree_root,
        format,
        &db,
        old_tree.as_ref(),
        &target_tree,
        commands::read_tree::UnpackPorcelain::Checkout,
        recurse_submodules,
        overwrite_untracked,
    )
}

fn checkout_is_dirty_tree_error(err: &GitError) -> bool {
    matches!(err, GitError::Transaction(msg) if msg.contains("clean working tree"))
}

fn detach_head_with_reflog(
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    message: Vec<u8>,
) -> Result<()> {
    let refs = FileRefStore::new(git_dir, format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid: ObjectId::null(format),
            new_oid: *target,
            committer: committer_identity_for_reflog()?,
            message,
        }),
    });
    tx.commit()
}

fn switch_head_symbolic_with_reflog(
    git_dir: &Path,
    format: ObjectFormat,
    branch: &str,
    target: &ObjectId,
    from: &str,
) -> Result<()> {
    let refs = FileRefStore::new(git_dir, format);
    let mut tx = refs.transaction();
    let message = format!("checkout: moving from {from} to {branch}").into_bytes();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(branch_ref_name(branch)?),
        reflog: Some(ReflogEntry {
            old_oid: *target,
            new_oid: *target,
            committer: committer_identity_for_reflog()?,
            message: message.clone(),
        }),
    });
    tx.commit()?;
    ensure_head_checkout_reflog_entry(&refs, format, target, message)
}

fn ensure_head_checkout_reflog_entry(
    refs: &FileRefStore,
    format: ObjectFormat,
    target: &ObjectId,
    message: Vec<u8>,
) -> Result<()> {
    if let Ok(entries) = refs.read_reflog("HEAD")
        && entries
            .last()
            .is_some_and(|entry| entry.new_oid == *target && entry.message == message)
    {
        return Ok(());
    }
    refs.append_reflog(
        "HEAD",
        &ReflogEntry {
            old_oid: *target,
            new_oid: *target,
            committer: committer_identity_for_reflog()?,
            message,
        },
    )
}

fn checkout_rollback_branch_update(
    git_dir: &Path,
    format: ObjectFormat,
    rollback: &Option<(String, Option<RefTarget>)>,
) {
    let Some((name, previous)) = rollback else {
        return;
    };
    let store = FileRefStore::new(git_dir, format);
    match previous {
        Some(target) => {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: name.clone(),
                expected: None,
                new: target.clone(),
                reflog: None,
            });
            let _ = tx.commit();
        }
        None => {
            let _ = store.delete_ref(name);
        }
    }
}

fn checkout_reflog_from_name(store: &FileRefStore) -> String {
    match store.read_ref("HEAD") {
        Ok(Some(RefTarget::Symbolic(name))) => name
            .strip_prefix("refs/heads/")
            .unwrap_or(&name)
            .to_string(),
        Ok(Some(RefTarget::Direct(oid))) => oid.to_hex(),
        _ => "HEAD".to_string(),
    }
}

enum CheckoutBranchMode {
    Existing,
    Detach,
    Create {
        branch: String,
        force: bool,
        orphan: bool,
    },
}

/// Resolve a `checkout -p` / `restore -p` tree-ish to the OID hex that the
/// `diff-index` modes pass on the command line. `<a>...<b>` ranges become their
/// merge base (git replaces them because `diff-index` does not parse `...`); a
/// plain ref/oid is handed to `diff-index` verbatim.
fn checkout_patch_resolve_revision(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<String> {
    if let Some((left, right)) = rev.split_once("...") {
        let left = if left.is_empty() { "HEAD" } else { left };
        let right = if right.is_empty() { "HEAD" } else { right };
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        let left = commands::diff::diff_resolve_commit_arg(git_dir, format, &db, left)?;
        let right = commands::diff::diff_resolve_commit_arg(git_dir, format, &db, right)?;
        let base = commands::diff::diff_single_merge_base(git_dir, format, &db, &left, &right)?;
        return Ok(base.to_hex());
    }
    Ok(rev.to_string())
}

/// Whether `arg` should be treated as the tree-ish operand of a `checkout -p`
/// (rather than a pathspec): an explicit `HEAD`/`@`, a `...` range, or anything
/// that resolves to a commit/tree-ish.
fn checkout_patch_arg_is_revision(git_dir: &Path, format: ObjectFormat, arg: &str) -> bool {
    arg == "HEAD"
        || arg == "@"
        || arg.contains("...")
        || checkout_resolve_start_oid(git_dir, format, arg).is_ok()
        || {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            commands::diff::diff_resolve_commit_arg(git_dir, format, &db, arg).is_ok()
        }
}

/// `git checkout -p [<tree-ish>] [--] [<pathspec>...]`: pick the patch mode from
/// the tree-ish (none / HEAD / other) and run the shared interactive engine.
fn checkout_run_patch(
    git_dir: &Path,
    format: ObjectFormat,
    positional: &[String],
    dashdash_index: Option<usize>,
    no_auto_advance: bool,
) -> Result<()> {
    let (rev, pathspecs): (Option<&str>, &[String]) = match dashdash_index {
        Some(index) => {
            let (before, after) = positional.split_at(index);
            match before {
                [] => (None, after),
                [rev] => (Some(rev.as_str()), after),
                _ => {
                    return Err(GitError::Command(
                        "checkout with multiple tree-ish arguments is not supported".into(),
                    ));
                }
            }
        }
        None => {
            if !positional.is_empty()
                && checkout_patch_arg_is_revision(git_dir, format, &positional[0])
            {
                (Some(positional[0].as_str()), &positional[1..])
            } else {
                (None, positional)
            }
        }
    };

    let (mode, revision): (commands::add_patch::PatchMode, Option<String>) = match rev {
        None => (commands::add_patch::PatchMode::CheckoutIndex, None),
        Some(rev) if rev == "HEAD" || rev == "@" => (
            commands::add_patch::PatchMode::CheckoutHead,
            Some("HEAD".to_string()),
        ),
        Some(rev) => (
            commands::add_patch::PatchMode::CheckoutNothead,
            Some(checkout_patch_resolve_revision(git_dir, format, rev)?),
        ),
    };

    let cfg =
        commands::add_interactive::resolve_patch_config(git_dir, None, None, !no_auto_advance)?;
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    commands::add_patch::run_add_patch(mode, pathspecs, revision.as_deref(), &mut handle, cfg)
}

/// `git restore -p [--source=<rev>] [--staged] [--worktree] [<pathspec>...]`:
/// pick the patch mode from `--staged`/`--worktree` and `--source`, then run the
/// shared interactive engine. Default restore (worktree only) discards from the
/// working tree; `--staged --worktree` touches both like `checkout -p`.
fn restore_run_patch(
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    source: Option<&str>,
    staged: bool,
    worktree: bool,
) -> Result<()> {
    let pathspecs: Vec<String> = paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let (mode, revision): (commands::add_patch::PatchMode, Option<String>) = if staged && !worktree
    {
        // `restore --staged -p` unstages selected hunks (ADD_P_RESET).
        (commands::add_patch::PatchMode::Reset, None)
    } else {
        // `--staged --worktree` touches the index too (ADD_P_CHECKOUT); the
        // default worktree restore touches only the working tree (ADD_P_WORKTREE).
        let touches_index = staged && worktree;
        match source {
            None => (commands::add_patch::PatchMode::CheckoutIndex, None),
            Some(rev) if rev == "HEAD" || rev == "@" => {
                let mode = if touches_index {
                    commands::add_patch::PatchMode::CheckoutHead
                } else {
                    commands::add_patch::PatchMode::WorktreeHead
                };
                (mode, Some("HEAD".to_string()))
            }
            Some(rev) => {
                let resolved = checkout_patch_resolve_revision(git_dir, format, rev)?;
                let mode = if touches_index {
                    commands::add_patch::PatchMode::CheckoutNothead
                } else {
                    commands::add_patch::PatchMode::WorktreeNothead
                };
                (mode, Some(resolved))
            }
        }
    };
    let cfg = commands::add_interactive::resolve_patch_config(git_dir, None, None, true)?;
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    commands::add_patch::run_add_patch(mode, &pathspecs, revision.as_deref(), &mut handle, cfg)
}

fn checkout_switch_to_unborn_branch(git_dir: &Path, branch: &str) -> Result<()> {
    let store = FileRefStore::new(git_dir, repository_object_format(git_dir)?);
    let name = branch_ref_name(branch)?;
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(name),
        reflog: None,
    });
    tx.commit()
}

enum CheckoutMessage {
    Existing { branch: String, announce: bool },
    New { branch: String },
    Reset { branch: String },
}

impl CheckoutMessage {
    fn branch(&self) -> &str {
        match self {
            Self::Existing { branch, .. } | Self::New { branch } | Self::Reset { branch } => branch,
        }
    }

    fn announces(&self) -> bool {
        match self {
            Self::Existing { announce, .. } => *announce,
            Self::New { .. } | Self::Reset { .. } => true,
        }
    }

    fn print(&self) {
        match self {
            Self::Existing { branch, announce } => {
                if *announce {
                    eprintln!("Switched to branch '{branch}'");
                }
            }
            Self::New { branch } => eprintln!("Switched to a new branch '{branch}'"),
            Self::Reset { branch } => eprintln!("Switched to and reset branch '{branch}'"),
        }
    }
}

fn detached_checkout_subject(git_dir: &Path, format: ObjectFormat, oid: &ObjectId) -> String {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let Ok(object) = db.read_object(oid) else {
        return String::new();
    };
    if object.object_type != ObjectType::Commit {
        return String::new();
    }
    let Ok(commit) = Commit::parse(format, &object.body) else {
        return String::new();
    };
    String::from_utf8_lossy(&commit.message)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}
