//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

/// git's `git reset --help`-derived usage block, printed after an `error:` line
/// when parse-options rejects an argument (matches builtin/reset.c's usage).
const RESET_USAGE: &str = "\
usage: git reset [--mixed | --soft | --hard | --merge | --keep] [-q] [<commit>]
   or: git reset [-q] [<tree-ish>] [--] <pathspec>...
   or: git reset [-q] [--pathspec-from-file [--pathspec-file-nul]] [<tree-ish>]
   or: git reset --patch [<tree-ish>] [--] [<pathspec>...]
";

pub(crate) fn cmd_reset(args: &[String]) -> Result<()> {
    let mut positionals = Vec::new();
    let mut quiet = false;
    let mut recurse_submodules = None;
    let mut mode = ResetMode::Mixed;
    let mut parsing_options = true;
    let mut saw_separator = false;
    let mut separator_index = None;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut intent_to_add = false;
    let mut patch = false;
    let mut no_auto_advance = false;
    let mut unified_context: Option<i64> = None;
    let mut inter_hunk_context: Option<i64> = None;
    // git's `reset --mixed` refreshes the index stat-cache by default; `--no-refresh`
    // leaves the freshly-restored entries stat-dirty so `git diff-files` shows them.
    let mut refresh = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            if pathspec_from_file.is_some() {
                eprintln!(
                    "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                );
                return Err(GitError::Exit(128));
            }
            positionals.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "--" => {
                parsing_options = false;
                saw_separator = true;
                separator_index = Some(positionals.len());
            }
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-N" | "--intent-to-add" => intent_to_add = true,
            "--no-intent-to-add" => intent_to_add = false,
            "-p" | "--patch" => patch = true,
            "--no-patch" => patch = false,
            "--no-auto-advance" => no_auto_advance = true,
            "-U" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(true);
                };
                patch_validate_unified_context(value, true)?;
                unified_context = value.parse::<i64>().ok();
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                let value = &value[2..];
                patch_validate_unified_context(value, true)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--unified" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(false);
                };
                patch_validate_unified_context(value, false)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                let value = &value["--unified=".len()..];
                patch_validate_unified_context(value, false)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--inter-hunk-context" => {
                let Some(value) = iter.next() else {
                    return commit_inter_hunk_context_requires_value_error();
                };
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = value.parse::<i64>().ok();
            }
            "--inter-hunk-context=" => {
                return commit_inter_hunk_context_expects_numerical_value_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                let value = &value["--inter-hunk-context=".len()..];
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = value.parse::<i64>().ok();
            }
            // A whole-tree `--mixed` reset restores index entries with a zeroed
            // cached stat (see `restored_head_index_entry`). git refreshes them
            // by default (re-stat + clear the stat-dirty state for unchanged
            // content) so `git diff-files` is clean; `--no-refresh` leaves them
            // stat-dirty so diff-files reports them `M` (t7102 cell 28).
            "--refresh" => refresh = true,
            "--no-refresh" => refresh = false,
            "--recurse-submodules" => recurse_submodules = Some(true),
            "--no-recurse-submodules" => recurse_submodules = Some(false),
            "--mixed" => mode = ResetMode::Mixed,
            "--soft" => mode = ResetMode::Soft,
            "--hard" => mode = ResetMode::Hard,
            "--merge" => mode = ResetMode::Merge,
            "--keep" => mode = ResetMode::Keep,
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("reset --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("reset --pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--end-of-options" => {
                // Everything after `--end-of-options` is a positional (commit-ish
                // or pathspec), even if it begins with a dash. git's parse-options
                // uses this to disambiguate a ref like `--foo` from an option.
                parsing_options = false;
            }
            value if value.starts_with('-') => {
                // Mirror git's parse-options error for an unrecognized flag:
                // `error: unknown option `<name>'` (long options drop the leading
                // `--`; a short cluster like `-o` keeps its single dash stripped),
                // followed by the usage block, exiting with parse-options' code 129.
                let name = value
                    .strip_prefix("--")
                    .or_else(|| value.strip_prefix('-'))
                    .unwrap_or(value);
                eprintln!("error: unknown option `{name}'");
                eprint!("{RESET_USAGE}");
                return Err(GitError::Exit(129));
            }
            value => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                positionals.push(value.to_string());
            }
        }
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if no_auto_advance && !patch {
        eprintln!("fatal: the option '--no-auto-advance' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if unified_context.is_some() && !patch {
        eprintln!("fatal: the option '--unified' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if inter_hunk_context.is_some() && !patch {
        eprintln!("fatal: the option '--inter-hunk-context' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if patch {
        let mut stdin = io::stdin().lock();
        let mut cfg = commands::add_patch::PatchConfig {
            auto_advance: !no_auto_advance,
            context: unified_context.map(|value| value as usize),
            interhunk: inter_hunk_context.map(|value| value as usize),
            ..commands::add_patch::PatchConfig::default()
        };
        cfg.reset_interactive =
            sley_config::read_repo_config(&discover_git_dir(&env::current_dir()?)?, None)
                .ok()
                .and_then(|config| {
                    config
                        .get("interactive", None, "reset")
                        .map(ToString::to_string)
                })
                .unwrap_or_default();
        return commands::add_patch::run_add_patch(
            commands::add_patch::PatchMode::Reset,
            &positionals,
            &mut stdin,
            cfg,
        );
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    // git's `setup_work_tree()` (builtin/reset.c): every reset that touches the
    // working tree — `--hard`, `--merge`, `--keep` — must run in a work tree, so
    // a bare repository refuses with "this operation must be run in a work
    // tree". `--soft` (HEAD-only) and `--mixed` (index-only) are exempt.
    if matches!(mode, ResetMode::Hard | ResetMode::Merge | ResetMode::Keep) {
        require_work_tree(&git_dir)?;
    }
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let reset_config = read_repo_config(&git_dir)?;
    let recurse_submodules = recurse_submodules.unwrap_or_else(|| {
        reset_config
            .get_bool("submodule", None, "recurse")
            .unwrap_or(false)
    });
    let pathspec_from_file_provided = pathspec_from_file.is_some();
    if mode == ResetMode::Merge {
        if pathspec_from_file_provided || (saw_separator && !positionals.is_empty()) {
            eprintln!("fatal: Cannot do merge reset with paths.");
            return Err(GitError::Exit(128));
        }
        let target = match positionals.as_slice() {
            [] => "HEAD",
            [target] => target.as_str(),
            _ => {
                eprintln!("fatal: Cannot do merge reset with paths.");
                return Err(GitError::Exit(128));
            }
        };
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let target_oid = resolve_revision_commitish(&git_dir, format, target)?;
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        return commands::replay::reset_merge_in(
            &git_dir,
            &worktree_root,
            format,
            Some(&target_commit),
        )
        .map_err(|err| match err {
            GitError::Command(message) => {
                eprintln!("fatal: {message}");
                GitError::Exit(128)
            }
            other => other,
        });
    }
    if mode == ResetMode::Keep {
        // git's `reset --keep` (builtin/reset.c): a two-way merge from the
        // current HEAD tree to the target tree, carrying forward local
        // modifications where safe and refusing the reset (with the read-tree
        // "not uptodate. Cannot merge." porcelain) when a touched file has
        // local changes. It never accepts paths.
        if pathspec_from_file_provided || (saw_separator && !positionals.is_empty()) {
            eprintln!("fatal: Cannot do keep reset with paths.");
            return Err(GitError::Exit(128));
        }
        let target = match positionals.as_slice() {
            [] => "HEAD",
            [target] => target.as_str(),
            _ => {
                eprintln!("fatal: Cannot do keep reset with paths.");
                return Err(GitError::Exit(128));
            }
        };
        // git's `die_if_unmerged_cache(KEEP)`: a pending merge (MERGE_HEAD) or
        // unmerged index entries forbid `--keep` (same gate as `--soft`).
        if reset_soft_blocked_by_merge(&git_dir, format)? {
            eprintln!("fatal: Cannot do a keep reset in the middle of a merge.");
            return Err(GitError::Exit(128));
        }
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let head_oid = resolve_revision(&git_dir, format, "HEAD").map_err(|_| {
            eprintln!("fatal: You do not have a valid HEAD.");
            GitError::Exit(128)
        })?;
        let old_head = head_oid;
        let head_tree = commands::merge_rebase::commit_tree_oid(&db, format, &head_oid)?;
        let target_oid = resolve_revision_commitish(&git_dir, format, target)?;
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        let target_tree = commands::merge_rebase::commit_tree_oid(&db, format, &target_commit)?;
        write_reset_orig_head(&git_dir, &old_head, format)?;
        // The structural lever: route `--keep` through the SAME twoway_merge
        // engine as checkout, with the read-tree abort wording its test asserts.
        // git's `reset_index(KEEP)`: a twoway_merge with `update=1` updates the
        // worktree (carrying forward safe local modifications, aborting on a
        // touched-file conflict). This may leave staged changes in the index.
        commands::read_tree::checkout_two_way_engine(
            &git_dir,
            &worktree_root,
            format,
            &db,
            Some(&head_tree),
            &target_tree,
            commands::read_tree::UnpackPorcelain::ReadTree,
            recurse_submodules,
            false,
        )?;
        // git's second pass: `if (reset_type == KEEP && !err) reset_index(MIXED)`
        // — an index-only reset to the target tree, so the resulting index
        // matches the target exactly (a staged-but-untouched change is dropped
        // from the index while its worktree content is preserved by pass 1).
        sley_worktree::reset_index_to_commit(
            worktree_root.clone(),
            git_dir.clone(),
            format,
            &target_commit,
        )?;
        refresh_reset_index(&worktree_root, &git_dir, format)?;
        update_reset_head_ref(
            &git_dir,
            format,
            old_head,
            target_commit,
            target,
            commit_identity_from_env("COMMITTER")?,
        )?;
        sley_sequencer::replay::remove_branch_state(&git_dir);
        return Ok(());
    }
    if matches!(mode, ResetMode::Soft | ResetMode::Hard) {
        if pathspec_from_file_provided {
            eprintln!("fatal: Cannot do {} reset with paths.", mode.as_str());
            return Err(GitError::Exit(128));
        }
        if saw_separator && !positionals.is_empty() {
            eprintln!("fatal: Cannot do {} reset with paths.", mode.as_str());
            return Err(GitError::Exit(128));
        }
        let target = match positionals.as_slice() {
            [] => "HEAD",
            [target] => target.as_str(),
            _ => {
                eprintln!("fatal: Cannot do {} reset with paths.", mode.as_str());
                return Err(GitError::Exit(128));
            }
        };
        // git refuses a `--soft` reset while a merge is in progress or the index
        // carries unmerged entries: a soft reset only moves HEAD, so leaving the
        // half-merged index behind would silently strand the conflict state.
        // builtin/reset.c: `reset_type == SOFT && (merge in progress || unmerged)`
        // → "Cannot do a soft reset in the middle of a merge." (exit 128).
        if mode == ResetMode::Soft && reset_soft_blocked_by_merge(&git_dir, format)? {
            eprintln!("fatal: Cannot do a soft reset in the middle of a merge.");
            return Err(GitError::Exit(128));
        }
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let old_head = match resolve_revision(&git_dir, format, "HEAD") {
            Ok(oid) => oid,
            Err(_) => zero_oid(format)?,
        };
        if mode == ResetMode::Hard
            && target == "HEAD"
            && resolve_revision(&git_dir, format, "HEAD").is_err()
        {
            // `git reset --hard` on an unborn branch: empty the index and
            // remove the (previously tracked) worktree files.
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
            sley_sequencer::replay::remove_branch_state(&git_dir);
            return Ok(());
        }
        let target_oid = resolve_revision_commitish(&git_dir, format, target)?;
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        write_reset_orig_head(&git_dir, &old_head, format)?;
        if mode == ResetMode::Hard {
            if recurse_submodules {
                commands::read_tree::reset_index_and_worktree_to_commit(
                    &worktree_root,
                    &git_dir,
                    format,
                    &target_commit,
                    true,
                )?;
            } else {
                sley_worktree::reset_index_and_worktree_to_commit(
                    worktree_root.clone(),
                    git_dir.clone(),
                    format,
                    &target_commit,
                )?;
            }
            apply_reset_sparse_checkout(&worktree_root, &git_dir, format)?;
        }
        update_reset_head_ref(
            &git_dir,
            format,
            old_head,
            target_commit,
            target,
            commit_identity_from_env("COMMITTER")?,
        )?;
        if mode == ResetMode::Hard && !quiet {
            print_reset_hard_head(&git_dir, format, &target_commit)?;
        }
        if mode == ResetMode::Hard {
            commands::merge_rebase::save_merge_autostash(&git_dir, format);
        }
        sley_sequencer::replay::remove_branch_state(&git_dir);
        return Ok(());
    }

    if !saw_separator
        && positionals.len() == 1
        && let Ok(target_oid) = resolve_revision_commitish(&git_dir, format, &positionals[0])
    {
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let old_head = match resolve_revision(&git_dir, format, "HEAD") {
            Ok(oid) => oid,
            Err(_) => zero_oid(format)?,
        };
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        // For `git reset -N` capture the paths the *current* index tracks that the
        // target tree does NOT, so they can be re-recorded as intent-to-add after
        // the index is reset (git: removed paths "will be added later").
        let ita_candidates = if intent_to_add {
            reset_intent_to_add_candidates(&git_dir, &db, format, &target_commit)?
        } else {
            Vec::new()
        };
        write_reset_orig_head(&git_dir, &old_head, format)?;
        sley_worktree::reset_index_to_commit(
            worktree_root.clone(),
            git_dir.clone(),
            format,
            &target_commit,
        )?;
        if intent_to_add && !ita_candidates.is_empty() {
            apply_reset_intent_to_add(&git_dir, format, &ita_candidates)?;
        }
        // git's `--mixed` reset refreshes the index by default: the restored
        // entries carry a zeroed cached stat, so without a refresh `git diff-files`
        // would report every unchanged tracked file as `M`. The refresh re-stats
        // each entry and clears the stat-dirty state where content still matches.
        // `--no-refresh` skips it, leaving the entries stat-dirty (t7102 cell 28).
        if refresh {
            refresh_reset_index(&worktree_root, &git_dir, format)?;
        }
        update_reset_head_ref(
            &git_dir,
            format,
            old_head,
            target_commit,
            &positionals[0],
            commit_identity_from_env("COMMITTER")?,
        )?;
        sley_sequencer::replay::remove_branch_state(&git_dir);
        if !quiet {
            print_reset_unstaged_changes(&worktree_root, &git_dir, format)?;
        }
        return Ok(());
    }

    // A bare `git reset` (whole-tree `--mixed`) on an unborn HEAD resets the
    // index to the empty tree: just clear it (git's `reset_index` against an
    // unborn HEAD) instead of treating the cwd as a pathspec that matches no
    // tracked file. `git checkout --orphan X && git reset` relies on this.
    if !saw_separator
        && positionals.is_empty()
        && !pathspec_from_file_provided
        && resolve_revision(&git_dir, format, "HEAD").is_err()
    {
        fs::write(
            sley_worktree::repository_index_path(&git_dir),
            Index {
                version: 2,
                entries: Vec::new(),
                extensions: Vec::new(),
                checksum: None,
            }
            .write(format)?,
        )?;
        sley_sequencer::replay::remove_branch_state(&git_dir);
        return Ok(());
    }

    let mut source_tree = None;
    let mut paths = if let Some(index) = separator_index {
        let (before_separator, after_separator) = positionals.split_at(index);
        match before_separator {
            [] => {}
            [target] => {
                let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                let target_oid = resolve_revision_treeish(&git_dir, format, target)?;
                source_tree = Some(sley_rev::peel_to_tree(&db, format, &target_oid)?);
            }
            _ => {
                eprintln!("fatal: Cannot do mixed reset with multiple trees.");
                return Err(GitError::Exit(128));
            }
        }
        after_separator
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    } else {
        let mut values = positionals;
        if values.len() > 1
            && let Ok(target_oid) = resolve_revision_treeish(&git_dir, format, &values[0])
        {
            let db = FileObjectDatabase::from_git_dir(&git_dir, format);
            source_tree = Some(sley_rev::peel_to_tree(&db, format, &target_oid)?);
            values.remove(0);
        }
        values
            .into_iter()
            .filter(|value| value != "HEAD")
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    };
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    let no_explicit_paths = paths.is_empty() && !pathspec_from_file_provided;
    if no_explicit_paths {
        paths.push(worktree_root.clone());
    }
    if !saw_separator && source_tree.is_none() {
        for path in &paths {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            if !absolute.exists() {
                return Err(sley_rev::ambiguous_argument_error(
                    &path.display().to_string(),
                ));
            }
        }
    }
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
    if let Some(tree_oid) = source_tree.as_ref() {
        sley_worktree::restore_index_paths_from_tree_allow_unmatched(
            worktree_root.clone(),
            git_dir.clone(),
            format,
            tree_oid,
            &resolved_paths,
        )?;
    } else {
        sley_worktree::restore_index_paths_from_head(
            worktree_root.clone(),
            git_dir.clone(),
            format,
            &resolved_paths,
        )?;
    }
    if no_explicit_paths {
        // A bare `git reset` (whole-tree mixed reset to HEAD) records ORIG_HEAD
        // just like the explicit-commit whole-tree paths. Pathspec resets do not.
        if let Ok(old_head) = resolve_revision(&git_dir, format, "HEAD") {
            write_reset_orig_head(&git_dir, &old_head, format)?;
        }
        // Whole-tree `--mixed` refreshes the stat-cache by default (see the
        // single-positional path above); `--no-refresh` leaves it stat-dirty.
        if refresh {
            refresh_reset_index(&worktree_root, &git_dir, format)?;
        }
        sley_sequencer::replay::remove_branch_state(&git_dir);
    }
    if !quiet {
        print_reset_unstaged_changes(&worktree_root, &git_dir, format)?;
    }
    Ok(())
}

/// Refresh the index stat-cache after a whole-tree `--mixed` reset (git's default
/// `--refresh` behaviour). The reset restores entries with a zeroed cached stat;
/// this re-stats each one and clears the stat-dirty state where content still
/// matches, so `git diff-files` reports a clean index. Mirrors git's
/// `refresh_index` call in `builtin/reset.c`: quiet (no "needs update" output, and
/// content mismatches are not an error — they are genuine worktree changes) and
/// tolerant of missing files (those are deletions, reported elsewhere).
fn refresh_reset_index(worktree_root: &Path, git_dir: &Path, format: ObjectFormat) -> Result<()> {
    sley_worktree::refresh_index_paths(
        worktree_root,
        git_dir,
        format,
        &[],
        /* quiet */ true,
        /* ignore_missing */ true,
        /* really_refresh */ false,
    )?;
    Ok(())
}

/// Record the pre-reset HEAD in `.git/ORIG_HEAD`, exactly as `git reset` does for
/// every whole-tree reset (all modes). Pathspec resets do NOT update ORIG_HEAD, so
/// this is only called on the whole-tree code paths. A null/unborn old HEAD writes
/// nothing — git leaves ORIG_HEAD untouched when there is no commit to record.
fn write_reset_orig_head(git_dir: &Path, old_head: &ObjectId, format: ObjectFormat) -> Result<()> {
    if *old_head == ObjectId::null(format) {
        return Ok(());
    }
    fs::write(git_dir.join("ORIG_HEAD"), format!("{old_head}\n"))?;
    Ok(())
}

/// The worktree-relative paths a `git reset -N` should re-record as intent-to-add:
/// stage-0 paths the *current* index tracks that the target tree does NOT (i.e. the
/// adds being un-staged by the reset). git keeps these as intent-to-add so the file
/// still shows in `git diff` but is absent from the written tree.
fn reset_intent_to_add_candidates(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    target_commit: &ObjectId,
) -> Result<Vec<BString>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    let target_tree = sley_rev::peel_to_tree(db, format, target_commit)?;
    let target_index = sley_worktree::index_from_tree(db, format, &target_tree)?;
    let target_paths: std::collections::BTreeSet<&BString> = target_index
        .entries
        .iter()
        .map(|entry| &entry.path)
        .collect();
    Ok(index
        .entries
        .iter()
        .filter(|entry| {
            entry.stage() == sley_index::Stage::Normal && !target_paths.contains(&entry.path)
        })
        .map(|entry| entry.path.clone())
        .collect())
}

/// Insert intent-to-add placeholders into the (already reset) index for the given
/// paths, matching `git reset -N`. Each becomes an ITA stage-0 entry, so the path
/// is reported by `git diff` yet excluded from `git write-tree`'s output.
fn apply_reset_intent_to_add(
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[BString],
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let existing: std::collections::BTreeSet<&BString> =
        index.entries.iter().map(|entry| &entry.path).collect();
    let additions = paths
        .iter()
        .filter(|path| !existing.contains(*path))
        .map(|path| IndexEntry::intent_to_add(format, path.clone()))
        .collect::<Vec<_>>();
    index.entries.extend(additions);
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    // intent-to-add entries carry extended flags, which the v2 index writer cannot
    // encode; bump to v3 when needed (mirrors `git add -N`'s index upgrade).
    index.upgrade_version_for_flags();
    fs::write(&index_path, index.write(format)?)?;
    Ok(())
}

/// The byte paths of every index entry carrying the skip-worktree bit, so the
/// post-reset "Unstaged changes" summary can exclude them (git never lists a
/// skip-worktree path there). Empty when the index is absent.
fn reset_skip_worktree_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<std::collections::BTreeSet<Vec<u8>>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(std::collections::BTreeSet::new());
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    Ok(index
        .entries
        .iter()
        .filter(|entry| entry.is_skip_worktree())
        .map(|entry| entry.path.to_vec())
        .collect())
}

/// Whether a `--soft` reset must be refused because a merge is in flight. git's
/// builtin/reset.c blocks a soft reset when `MERGE_HEAD` exists OR the index holds
/// any unmerged (stage != 0) entry — a soft reset only moves HEAD, so it would
/// otherwise abandon the half-resolved index. Returns true in either case.
fn reset_soft_blocked_by_merge(git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    if git_dir.join("MERGE_HEAD").is_file() {
        return Ok(true);
    }
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(false);
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    Ok(index
        .entries
        .iter()
        .any(|entry| entry.stage() != sley_index::Stage::Normal))
}

fn apply_reset_sparse_checkout(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let worktree_config = GitConfig::read(git_dir.join("config.worktree")).unwrap_or_default();
    let repo_config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    let sparse_enabled = worktree_config
        .get_bool("core", None, "sparseCheckout")
        .or_else(|| repo_config.get_bool("core", None, "sparseCheckout"))
        .unwrap_or(false);
    if !sparse_enabled {
        return Ok(());
    }
    let sparse_file = git_dir.join("info").join("sparse-checkout");
    if !sparse_file.exists() {
        return Ok(());
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
    let mut patterns: Vec<Vec<u8>> = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if patterns.last().map(Vec::is_empty) == Some(true) {
        patterns.pop();
    }
    let mode = if cone && commands::sparse_checkout::cone_patterns_are_valid(&patterns, true) {
        sley_worktree::SparseCheckoutMode::Cone
    } else {
        sley_worktree::SparseCheckoutMode::Full
    };
    let sparse = sley_worktree::SparseCheckout {
        patterns,
        sparse_index,
    };
    sley_worktree::apply_sparse_checkout_with_mode(worktree_root, git_dir, format, &sparse, mode)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResetMode {
    Mixed,
    Soft,
    Hard,
    Merge,
    Keep,
}

impl ResetMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Soft => "soft",
            Self::Hard => "hard",
            Self::Merge => "merge",
            Self::Keep => "keep",
        }
    }
}

fn print_reset_unstaged_changes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let mut entries = crate::collect_short_status(worktree_root, git_dir, format)?;
    entries.retain(|entry| matches!(entry.worktree, b'M' | b'D'));
    // git's post-reset summary omits skip-worktree paths: `update_index_refresh`
    // never marks a CE_SKIP_WORKTREE entry stat-dirty, so it never appears in the
    // "Unstaged changes after reset:" list (t7102 cell 29). Drop those paths.
    let skip_worktree = reset_skip_worktree_paths(git_dir, format)?;
    if !skip_worktree.is_empty() {
        entries.retain(|entry| !skip_worktree.contains(entry.path.as_slice()));
    }
    if entries.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Unstaged changes after reset:")?;
    for entry in entries {
        writeln!(
            stdout,
            "{}\t{}",
            entry.worktree as char,
            String::from_utf8_lossy(&entry.path)
        )?;
    }
    Ok(())
}
