//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_reset(args: &[String]) -> Result<()> {
    let mut positionals = Vec::new();
    let mut quiet = false;
    let mut mode = ResetMode::Mixed;
    let mut parsing_options = true;
    let mut saw_separator = false;
    let mut separator_index = None;
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
            "--refresh" | "--no-refresh" | "--no-recurse-submodules" => {}
            "--mixed" => mode = ResetMode::Mixed,
            "--soft" => mode = ResetMode::Soft,
            "--hard" => mode = ResetMode::Hard,
            "--merge" => mode = ResetMode::Merge,
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
            "HEAD" => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported reset option {value}"
                )));
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
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
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
        let target_oid = resolve_revision(&git_dir, format, target)?;
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
        let target_oid = resolve_revision(&git_dir, format, target)?;
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        if mode == ResetMode::Hard {
            sley_worktree::reset_index_and_worktree_to_commit(
                worktree_root.clone(),
                git_dir.clone(),
                format,
                &target_commit,
            )?;
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
        sley_sequencer::replay::remove_branch_state(&git_dir);
        return Ok(());
    }

    if !saw_separator
        && positionals.len() == 1
        && let Ok(target_oid) = resolve_revision(&git_dir, format, &positionals[0])
    {
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let old_head = match resolve_revision(&git_dir, format, "HEAD") {
            Ok(oid) => oid,
            Err(_) => zero_oid(format)?,
        };
        let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
        sley_worktree::reset_index_to_commit(
            worktree_root.clone(),
            git_dir.clone(),
            format,
            &target_commit,
        )?;
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

    let mut source_tree = None;
    let mut paths = if let Some(index) = separator_index {
        let (before_separator, after_separator) = positionals.split_at(index);
        match before_separator {
            [] => {}
            [target] => {
                let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                let target_oid = resolve_revision(&git_dir, format, target)?;
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
            && let Ok(target_oid) = resolve_revision(&git_dir, format, &values[0])
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
                eprintln!(
                    "fatal: ambiguous argument '{}': unknown revision or path not in the working tree.",
                    path.display()
                );
                eprintln!(
                    "Use '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
                );
                return Err(GitError::Exit(128));
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
        sley_worktree::restore_index_paths_from_tree(
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
        sley_sequencer::replay::remove_branch_state(&git_dir);
    }
    if !quiet {
        print_reset_unstaged_changes(&worktree_root, &git_dir, format)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResetMode {
    Mixed,
    Soft,
    Hard,
    Merge,
}

impl ResetMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Soft => "soft",
            Self::Hard => "hard",
            Self::Merge => "merge",
        }
    }
}

fn print_reset_unstaged_changes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let mut entries = sley_worktree::short_status(worktree_root, git_dir, format)?;
    entries.retain(|entry| matches!(entry.worktree, b'M' | b'D'));
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

pub(crate) fn cmd_checkout(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut force = false;
    let mut branch_mode = CheckoutBranchMode::Existing;
    let mut positional = Vec::new();
    let mut dashdash_index = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "--progress"
            | "--no-progress"
            | "--guess"
            | "--no-guess"
            | "--ignore-other-worktrees"
            | "--no-ignore-other-worktrees"
            | "--no-recurse-submodules" => {}
            "-b" => {
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
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("checkout -B requires a branch".into()))?;
                branch_mode = CheckoutBranchMode::Create {
                    branch: branch.to_string(),
                    force: true,
                    orphan: false,
                };
            }
            "--detach" => branch_mode = CheckoutBranchMode::Detach,
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

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;

    // Pathspec checkout: `checkout [<tree-ish>] [--] <pathspec>...` restores
    // paths (and, with a tree-ish, index entries) instead of switching HEAD.
    if matches!(branch_mode, CheckoutBranchMode::Existing) {
        let (source, paths): (Option<&str>, &[String]) = match dashdash_index {
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
            None if positional.len() > 1 => (Some(positional[0].as_str()), &positional[1..]),
            None if positional.len() == 1 => {
                // A single arg that is neither a branch nor a revision but
                // names an existing file is a path checkout.
                let value = &positional[0];
                let store = FileRefStore::new(&git_dir, format);
                let is_branch = branch_ref_name(value)
                    .ok()
                    .and_then(|name| sley_refs::resolve_ref_peeled(&store, &name).ok().flatten())
                    .is_some();
                if !is_branch
                    && sley_rev::resolve_revision(&git_dir, format, value).is_err()
                    && cwd.join(value).exists()
                {
                    (None, positional.as_slice())
                } else {
                    (None, &[])
                }
            }
            None => (None, &[]),
        };
        if !paths.is_empty() {
            let resolved_paths: Vec<PathBuf> = paths
                .iter()
                .map(|path| {
                    let path = PathBuf::from(path);
                    if path.is_absolute() { path } else { cwd.join(path) }
                })
                .collect();
            match source {
                Some(rev) => {
                    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                    let oid = resolve_revision(&git_dir, format, rev)?;
                    let tree = sley_rev::peel_to_tree(&db, format, &oid)?;
                    sley_worktree::restore_index_and_worktree_paths_from_tree(
                        worktree_root,
                        git_dir,
                        format,
                        &tree,
                        &resolved_paths,
                    )?;
                }
                None => {
                    let config = read_repo_config(&git_dir)?;
                    sley_worktree::restore_worktree_paths_filtered(
                        worktree_root,
                        &git_dir,
                        format,
                        &resolved_paths,
                        &config,
                    )?;
                }
            }
            return Ok(());
        }
    }

    // `git checkout -f <commit-ish>` where the target is the commit HEAD already
    // points at (the common `git checkout -f HEAD` form, e.g. the trailing step
    // of upstream's test_commit_bulk): force-restore the index and working tree
    // to that commit without changing which branch HEAD is on, and stay silent on
    // success — exactly git's behavior when no branch switch happens.
    if force
        && matches!(branch_mode, CheckoutBranchMode::Existing)
        && positional.len() == 1
    {
        let target = &positional[0];
        let store = FileRefStore::new(&git_dir, format);
        let head_commit = resolve_ref_peeled(&store, "HEAD")?;
        if let Ok(target_oid) = sley_rev::resolve_revision(&git_dir, format, target)
            && head_commit == Some(target_oid)
        {
            sley_worktree::reset_index_and_worktree_to_commit(
                &worktree_root,
                &git_dir,
                format,
                &target_oid,
            )?;
            sley_sequencer::replay::remove_branch_state(&git_dir);
            return Ok(());
        }
    }

    // `-f`: discard local index/worktree changes (including conflict stages)
    // before switching, so the clean-tree checkout below succeeds — git's
    // force semantics. Untracked files are preserved.
    if force {
        let store = FileRefStore::new(&git_dir, format);
        if let Ok(Some(head_oid)) = resolve_ref_peeled(&store, "HEAD") {
            sley_worktree::reset_index_and_worktree_to_commit(
                &worktree_root,
                &git_dir,
                format,
                &head_oid,
            )?;
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

    let checkout_message = match branch_mode {
        CheckoutBranchMode::Detach => {
            if positional.len() > 1 {
                return Err(GitError::Command(
                    "checkout --detach accepts at most one commit".into(),
                ));
            }
            let target = positional.first().map(String::as_str).unwrap_or("HEAD");
            let target_oid = resolve_revision(&git_dir, format, target)?;
            let db = FileObjectDatabase::from_git_dir(&git_dir, format);
            let target_oid = sley_rev::peel_to_commit(&db, format, &target_oid)?;
            let store = FileRefStore::new(&git_dir, format);
            let from = checkout_reflog_from_name(&store);
            let config = read_repo_config(&git_dir)?;
            let subject = detached_checkout_subject(&git_dir, format, &target_oid);
            let message = format!("checkout: moving from {from} to {target}").into_bytes();
            match sley_worktree::checkout_detached_filtered(
                &worktree_root,
                &git_dir,
                format,
                &target_oid,
                commit_identity_from_env("COMMITTER")?,
                message.clone(),
                &config,
            ) {
                Ok(_) => {}
                Err(err) if checkout_is_dirty_tree_error(&err) => {
                    checkout_twoway_dirty(&git_dir, &worktree_root, format, Some(&target_oid))?;
                    detach_head_with_reflog(&git_dir, format, &target_oid, message)?;
                }
                Err(err) => return Err(err),
            }
            sley_sequencer::replay::remove_branch_state(&git_dir);
            if !quiet {
                eprintln!(
                    "HEAD is now at {} {}",
                    format_log_abbrev_oid(&target_oid),
                    subject
                );
            }
            return Ok(());
        }
        CheckoutBranchMode::Existing => {
            let [branch] = positional.as_slice() else {
                return Err(GitError::Command(
                    "checkout currently supports: checkout [-q] <branch> or checkout [-q] -b|-B <branch> [<start>]".into(),
                ));
            };
            // A target that is not an existing branch but resolves to a commit-ish
            // (e.g. `A^0`, a tag, a raw oid) is a *detached HEAD* checkout, not a
            // branch switch. git detaches HEAD at the resolved commit; treating it
            // as a branch name would mint a bogus `refs/heads/A^0` symref.
            let store = FileRefStore::new(&git_dir, format);
            let is_branch = sley_refs::resolve_ref_peeled(&store, &branch_ref_name(branch)?)
                .ok()
                .flatten()
                .is_some();
            if !is_branch
                && let Ok(target_oid) = sley_rev::resolve_revision(&git_dir, format, branch)
            {
                let config = read_repo_config(&git_dir)?;
                let subject = detached_checkout_subject(&git_dir, format, &target_oid);
                let from = checkout_reflog_from_name(&store);
                let message =
                    format!("checkout: moving from {from} to {branch}").into_bytes();
                match sley_worktree::checkout_detached_filtered(
                    &worktree_root,
                    &git_dir,
                    format,
                    &target_oid,
                    commit_identity_from_env("COMMITTER")?,
                    message.clone(),
                    &config,
                ) {
                    Ok(_) => {}
                    Err(err) if checkout_is_dirty_tree_error(&err) => {
                        checkout_twoway_dirty(&git_dir, &worktree_root, format, Some(&target_oid))?;
                        detach_head_with_reflog(&git_dir, format, &target_oid, message)?;
                    }
                    Err(err) => return Err(err),
                }
                sley_sequencer::replay::remove_branch_state(&git_dir);
                if !quiet {
                    eprintln!(
                        "HEAD is now at {} {}",
                        format_log_abbrev_oid(&target_oid),
                        subject
                    );
                }
                return Ok(());
            }
            CheckoutMessage::Existing {
                branch: branch.clone(),
            }
        }
        CheckoutBranchMode::Create {
            branch,
            force,
            orphan,
        } => {
            if orphan {
                if !positional.is_empty() {
                    return Err(GitError::Command(
                        "checkout --orphan does not accept a start point".into(),
                    ));
                }
                checkout_switch_to_unborn_branch(&git_dir, &branch)?;
                sley_sequencer::replay::remove_branch_state(&git_dir);
                if !quiet {
                    eprintln!("Switched to a new branch '{branch}'");
                }
                return Ok(());
            }
            if positional.len() > 1 {
                return Err(GitError::Command(
                    "checkout -b/-B accepts at most one start point".into(),
                ));
            }
            let start = positional.first().map(String::as_str).unwrap_or("HEAD");
            let was_reset = checkout_create_or_reset_branch(
                &git_dir,
                format,
                &branch,
                start,
                force,
                commit_identity_from_env("COMMITTER")?,
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
    match sley_worktree::checkout_branch_filtered(
        &worktree_root,
        git_dir.clone(),
        format,
        branch,
        commit_identity_from_env("COMMITTER")?,
        &config,
    ) {
        Ok(_) => {}
        Err(err) if checkout_is_dirty_tree_error(&err) => {
            let store = FileRefStore::new(&git_dir, format);
            let from = checkout_reflog_from_name(&store);
            let target = sley_refs::resolve_ref_peeled(&store, &branch_ref_name(branch)?)?
                .ok_or_else(|| GitError::reference_not_found("branch"))?;
            checkout_twoway_dirty(&git_dir, &worktree_root, format, Some(&target))?;
            switch_head_symbolic_with_reflog(&git_dir, format, branch, &target, &from)?;
        }
        Err(err) => return Err(err),
    }
    sley_sequencer::replay::remove_branch_state(&git_dir);
    if !quiet {
        checkout_message.print();
    }
    Ok(())
}

pub(crate) fn cmd_switch(args: &[String]) -> Result<()> {
    // `switch --orphan <name>` clears the index and worktree of files carried
    // from the old HEAD (a twoway checkout to the empty tree), unlike
    // `checkout --orphan` which keeps them staged.
    if let Some(pos) = args.iter().position(|arg| arg == "--orphan") {
        let Some(branch) = args.get(pos + 1) else {
            return Err(GitError::Command("switch --orphan requires a branch".into()));
        };
        let cwd = env::current_dir()?;
        let git_dir = discover_git_dir(&cwd)?;
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        let format = repository_object_format(&git_dir)?;
        checkout_twoway_dirty(&git_dir, &worktree_root, format, None)?;
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
            "--quiet"
            | "--no-quiet"
            | "--progress"
            | "--no-progress"
            | "--overlay"
            | "--no-overlay"
            | "--ignore-unmerged"
            | "--no-ignore-unmerged"
            | "--ignore-skip-worktree-bits"
            | "--no-ignore-skip-worktree-bits"
            | "--no-recurse-submodules" => {}
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
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    if paths.is_empty() {
        return Err(GitError::Command(
            "restore requires at least one path".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
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
        if let Some(tree_oid) = source_tree.as_ref() {
            sley_worktree::restore_index_and_worktree_paths_from_tree(
                worktree_root,
                git_dir,
                format,
                tree_oid,
                &resolved_paths,
            )?;
        } else {
            sley_worktree::restore_index_and_worktree_paths_from_head(
                worktree_root,
                git_dir,
                format,
                &resolved_paths,
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
        sley_worktree::restore_worktree_paths(worktree_root, git_dir, format, &resolved_paths)?;
    }
    Ok(())
}

/// Dirty-tolerant two-way checkout fallback: paths whose content is the same
/// in the current HEAD and the target carry their index/worktree state across
/// the switch (git's twoway_merge); paths that must change are updated only
/// when clean, and staged changes that conflict with the target refuse the
/// switch. `target` of `None` switches to the empty tree (`switch --orphan`).
fn checkout_twoway_dirty(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    target: Option<&ObjectId>,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let target_map = match target {
        Some(target) => {
            let tree = commands::merge_rebase::commit_tree_oid(&db, format, target)?;
            stash_tree_entry_map(&db, format, &tree)?
        }
        None => BTreeMap::new(),
    };
    let refs = FileRefStore::new(git_dir, format);
    let head_map = match commands::merge_rebase::head_commit_oid(&refs)? {
        Some(head) => {
            let tree = commands::merge_rebase::commit_tree_oid(&db, format, &head)?;
            stash_tree_entry_map(&db, format, &tree)?
        }
        None => BTreeMap::new(),
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
    let old_index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let mut stage0: BTreeMap<Vec<u8>, IndexEntry> = BTreeMap::new();
    for entry in &old_index.entries {
        if index_entry_stage(entry) > 0 {
            eprintln!("error: you need to resolve your current index first");
            return Err(GitError::Exit(1));
        }
        stage0.insert(entry.path.clone().into_bytes(), entry.clone());
    }
    let all_paths: BTreeSet<Vec<u8>> = stage0
        .keys()
        .cloned()
        .chain(target_map.keys().cloned())
        .chain(head_map.keys().cloned())
        .collect();
    let mut blocked: Vec<Vec<u8>> = Vec::new();
    let mut updates: Vec<(Vec<u8>, (u32, ObjectId))> = Vec::new();
    let mut deletions: Vec<Vec<u8>> = Vec::new();
    // Paths the new index keeps from the old index even though the target
    // tree disagrees (the twoway "carry" rule: HEAD and target agree).
    let mut carried: BTreeSet<Vec<u8>> = BTreeSet::new();
    for path in &all_paths {
        let current = stage0.get(path).map(|entry| (entry.mode, entry.oid));
        let wanted = target_map.get(path).copied();
        let in_head = head_map.get(path).copied();
        if in_head == wanted {
            // Same on both sides of the switch: carry local state verbatim.
            if current.is_some() && wanted != current {
                carried.insert(path.clone());
            }
            continue;
        }
        if current == wanted {
            continue;
        }
        if current != in_head {
            // Staged (or missing) state conflicts with the switch.
            blocked.push(path.clone());
            continue;
        }
        let rel = String::from_utf8_lossy(path).into_owned();
        let full = worktree_root.join(&rel);
        if let Some(entry) = stage0.get(path) {
            if let Ok(bytes) = fs::read(&full) {
                let on_disk = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
                if on_disk != entry.oid {
                    blocked.push(path.clone());
                    continue;
                }
            }
        } else if let Some((_, oid)) = &wanted
            && let Ok(bytes) = fs::read(&full)
        {
            // Untracked file in the way: only identical content may be adopted.
            let on_disk = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
            if &on_disk != oid {
                eprintln!(
                    "error: The following untracked working tree files would be overwritten by checkout:"
                );
                eprintln!("\t{rel}");
                eprintln!("Please move or remove them before you switch branches.");
                eprintln!("Aborting");
                return Err(GitError::Exit(1));
            }
        }
        match wanted {
            Some(entry) => updates.push((path.clone(), entry)),
            None => deletions.push(path.clone()),
        }
    }
    if !blocked.is_empty() {
        eprintln!(
            "error: Your local changes to the following files would be overwritten by checkout:"
        );
        for path in &blocked {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please commit your changes or stash them before you switch branches.");
        eprintln!("Aborting");
        return Err(GitError::Exit(1));
    }
    for (path, (mode, oid)) in &updates {
        let content = commands::merge_rebase::merge_read_blob(&db, oid)?;
        commands::merge_rebase::merge_write_worktree_file(worktree_root, path, &content, *mode)?;
    }
    for path in &deletions {
        commands::merge_rebase::merge_remove_worktree_file(worktree_root, path)?;
    }
    let mut entries: Vec<IndexEntry> = Vec::new();
    for (path, (mode, oid)) in &target_map {
        if carried.contains(path) {
            continue;
        }
        if let Some(old) = stage0.get(path)
            && old.mode == *mode
            && old.oid == *oid
        {
            entries.push(old.clone());
        } else {
            entries.push(commands::merge_rebase::merge_index_entry(
                path, *mode, *oid, 0,
            ));
        }
    }
    for path in &carried {
        if let Some(old) = stage0.get(path) {
            entries.push(old.clone());
        }
    }
    // Staged adds whose path is in neither tree are carried too.
    for (path, entry) in &stage0 {
        if !target_map.contains_key(path)
            && !head_map.contains_key(path)
            && !carried.contains(path)
        {
            entries.push(entry.clone());
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        &index_path,
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
            committer: commit_identity_from_env("COMMITTER")?,
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
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(branch_ref_name(branch)?),
        reflog: Some(ReflogEntry {
            old_oid: *target,
            new_oid: *target,
            committer: commit_identity_from_env("COMMITTER")?,
            message: format!("checkout: moving from {from} to {branch}").into_bytes(),
        }),
    });
    tx.commit()
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
    Existing { branch: String },
    New { branch: String },
    Reset { branch: String },
}

impl CheckoutMessage {
    fn branch(&self) -> &str {
        match self {
            Self::Existing { branch } | Self::New { branch } | Self::Reset { branch } => branch,
        }
    }

    fn print(&self) {
        match self {
            Self::Existing { branch } => eprintln!("Switched to branch '{branch}'"),
            Self::New { branch } => eprintln!("Switched to a new branch '{branch}'"),
            Self::Reset { branch } => eprintln!("Switched to and reset branch '{branch}'"),
        }
    }
}

enum CommitShortFlag {
    /// A boolean flag that takes no value (e.g. `-q`, `-s`, `-a`).
    Boolean,
    /// A flag whose value is required (e.g. `-m`, `-F`, `-C`, `-c`, `-t`,
    /// `-U`). In a cluster it consumes the rest of the cluster; standalone it
    /// consumes the next argument.
    RequiresValue,
    /// A flag whose value is optional (`-S`, `-u`; `PARSE_OPT_OPTARG`). It
    /// consumes the rest of the cluster if any, but never the next argument.
    OptionalValue,
}

fn commit_short_flag_kind(ch: char) -> Option<CommitShortFlag> {
    match ch {
        // OPT__QUIET / OPT__VERBOSE and the plain OPT_BOOL entries.
        'q' | 'v' | 's' | 'e' | 'a' | 'i' | 'p' | 'o' | 'n' | 'z' => {
            Some(CommitShortFlag::Boolean)
        }
        // OPT_CALLBACK('m'), OPT_FILENAME('F'/'t'), OPT_STRING('c'/'C'),
        // OPT_DIFF_UNIFIED ('U').
        'm' | 'F' | 'c' | 'C' | 't' | 'U' => Some(CommitShortFlag::RequiresValue),
        // PARSE_OPT_OPTARG entries: gpg-sign ('S') and untracked-files ('u').
        'S' | 'u' => Some(CommitShortFlag::OptionalValue),
        _ => None,
    }
}

fn expand_commit_short_clusters(args: &[String]) -> Result<Vec<String>> {
    let mut expanded = Vec::with_capacity(args.len());
    let mut saw_dashdash = false;
    for arg in args {
        if saw_dashdash {
            expanded.push(arg.clone());
            continue;
        }
        if arg == "--" {
            saw_dashdash = true;
            expanded.push(arg.clone());
            continue;
        }
        let bytes = arg.as_bytes();
        // Not a short-option cluster: keep `-`, `--long`, and positionals as-is.
        if bytes.len() < 2 || bytes[0] != b'-' || bytes[1] == b'-' {
            expanded.push(arg.clone());
            continue;
        }
        let cluster = &arg[1..];
        let mut chars = cluster.char_indices();
        let Some((_, first)) = chars.next() else {
            expanded.push(arg.clone());
            continue;
        };
        // Only expand clusters that *start* with a boolean flag. If the first
        // flag is unknown or already takes a value, defer entirely to the main
        // parser (its glued-value / error arms own that input).
        if !matches!(commit_short_flag_kind(first), Some(CommitShortFlag::Boolean)) {
            expanded.push(arg.clone());
            continue;
        }
        expanded.push(format!("-{first}"));
        // Walk the remaining flags in this cluster. A value-taking flag
        // swallows the rest of the cluster and ends the scan; the main parser
        // owns next-argument consumption when the glued value is empty.
        for (idx, ch) in chars {
            match commit_short_flag_kind(ch) {
                Some(CommitShortFlag::Boolean) => expanded.push(format!("-{ch}")),
                Some(CommitShortFlag::RequiresValue)
                | Some(CommitShortFlag::OptionalValue) => {
                    // `-q` `m` `rest` -> `-mrest`; when `rest` is empty we emit
                    // just `-m`, and the main parser consumes the next argument
                    // (required) or treats the value as absent (optional).
                    expanded.push(format!("-{}", &cluster[idx..]));
                    break;
                }
                None => {
                    // Unknown flag inside the cluster: preserve the existing
                    // error for the whole original cluster (exit 1) rather than
                    // emitting partial side effects from the leading flags.
                    return Err(GitError::Command(format!(
                        "unsupported commit argument {arg}; currently supports -m and -F"
                    )));
                }
            }
        }
    }
    Ok(expanded)
}

pub(crate) fn cmd_commit(raw_args: &[String]) -> Result<()> {
    let args = expand_commit_short_clusters(raw_args)?;
    let args = args.as_slice();
    let mut message_chunks = Vec::new();
    let mut file_message = None;
    let mut signoff = false;
    let mut quiet = false;
    let mut allow_empty = false;
    let mut allow_empty_message = false;
    let mut all = false;
    let mut author_override = None;
    let mut author_date = None;
    let mut reuse_message = None;
    let mut reedit_message = false;
    let mut fixup_commit = None;
    let mut squash_commit = None;
    let mut trailers = Vec::new();
    let mut reset_author = false;
    let mut amend = false;
    let mut cleanup_mode = None;
    let mut include_without_paths = false;
    let mut only_without_paths = false;
    let mut status_mode = CommitStatusMode::Normal;
    let mut status_null = false;
    let mut null_implied_status = false;
    let mut dry_run = false;
    let mut interactive = false;
    let mut patch = false;
    let mut gpg_sign = false;
    let mut unified_context = false;
    let mut inter_hunk_context = false;
    let mut pathspec_from_file = None;
    let mut pathspec_from_file_active = false;
    let mut pathspec_file_nul = false;
    let mut pathspec_args = Vec::new();
    let mut edit_flag: Option<bool> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-m" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                let mut chunk = value.as_bytes()[2..].to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("-am") => {
                all = true;
                let message = if value.len() > 3 {
                    &value[3..]
                } else {
                    let Some(message) = iter.next() else {
                        return commit_message_requires_value_error();
                    };
                    message
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            "--message" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("--message=") => {
                let mut chunk = value.as_bytes()["--message=".len()..].to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            "--no-message" => message_chunks.clear(),
            value if value.starts_with("--no-message=") => {
                return commit_option_takes_no_value_error("no-message");
            }
            "-F" | "--file" => {
                let Some(path) = iter.next() else {
                    return commit_tree_file_requires_value_error();
                };
                file_message = Some(read_porcelain_commit_message_file(path)?);
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                file_message = Some(read_porcelain_commit_message_file(&value[2..])?);
            }
            value if value.starts_with("--file=") => {
                file_message = Some(read_porcelain_commit_message_file(
                    &value["--file=".len()..],
                )?);
            }
            "--no-file" => {}
            value if value.starts_with("--no-file=") => {
                return commit_option_takes_no_value_error("no-file");
            }
            "-C" | "--reuse-message" => {
                let Some(value) = iter.next() else {
                    return commit_reuse_message_requires_value_error(arg == "-C", false);
                };
                reuse_message = Some(value.to_string());
                reedit_message = false;
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                reuse_message = Some(value[2..].to_string());
                reedit_message = false;
            }
            value if value.starts_with("--reuse-message=") => {
                reuse_message = Some(value["--reuse-message=".len()..].to_string());
                reedit_message = false;
            }
            "--no-reuse-message" => {
                reuse_message = None;
                reedit_message = false;
            }
            value if value.starts_with("--no-reuse-message=") => {
                return commit_option_takes_no_value_error("no-reuse-message");
            }
            "-c" | "--reedit-message" => {
                let Some(value) = iter.next() else {
                    return commit_reuse_message_requires_value_error(arg == "-c", true);
                };
                reuse_message = Some(value.to_string());
                reedit_message = true;
            }
            value if value.starts_with("-c") && value.len() > 2 => {
                reuse_message = Some(value[2..].to_string());
                reedit_message = true;
            }
            value if value.starts_with("--reedit-message=") => {
                reuse_message = Some(value["--reedit-message=".len()..].to_string());
                reedit_message = true;
            }
            "--no-reedit-message" => {
                reuse_message = None;
                reedit_message = false;
            }
            value if value.starts_with("--no-reedit-message=") => {
                return commit_option_takes_no_value_error("no-reedit-message");
            }
            "--fixup" => {
                let Some(value) = iter.next() else {
                    return commit_fixup_requires_value_error();
                };
                fixup_commit = Some(CommitFixup::parse(value)?);
            }
            value if value.starts_with("--fixup=") => {
                fixup_commit = Some(CommitFixup::parse(&value["--fixup=".len()..])?);
            }
            "--no-fixup" => fixup_commit = None,
            value if value.starts_with("--no-fixup=") => {
                return commit_option_takes_no_value_error("no-fixup");
            }
            "--squash" => {
                let Some(value) = iter.next() else {
                    return commit_squash_requires_value_error();
                };
                squash_commit = Some(value.to_string());
            }
            value if value.starts_with("--squash=") => {
                squash_commit = Some(value["--squash=".len()..].to_string());
            }
            "--no-squash" => squash_commit = None,
            value if value.starts_with("--no-squash=") => {
                return commit_option_takes_no_value_error("no-squash");
            }
            "--trailer" => {
                let Some(value) = iter.next() else {
                    return commit_trailer_requires_value_error();
                };
                trailers.push(commands::tag::parse_tag_trailer(value));
            }
            value if value.starts_with("--trailer=") => {
                trailers.push(parse_tag_trailer(&value["--trailer=".len()..]));
            }
            "--no-trailer" => trailers.clear(),
            value if value.starts_with("--no-trailer=") => {
                return commit_option_takes_no_value_error("no-trailer");
            }
            "--reset-author" => reset_author = true,
            "--no-reset-author" => reset_author = false,
            value if value.starts_with("--reset-author=") => {
                return commit_option_takes_no_value_error("reset-author");
            }
            value if value.starts_with("--no-reset-author=") => {
                return commit_option_takes_no_value_error("no-reset-author");
            }
            "--amend" => amend = true,
            "--no-amend" => amend = false,
            value if value.starts_with("--amend=") => {
                return commit_option_takes_no_value_error("amend");
            }
            value if value.starts_with("--no-amend=") => {
                return commit_option_takes_no_value_error("no-amend");
            }
            "-s" | "--signoff" => signoff = true,
            "--no-signoff" => signoff = false,
            value if value.starts_with("--signoff=") => {
                return commit_option_takes_no_value_error("signoff");
            }
            value if value.starts_with("--no-signoff=") => {
                return commit_option_takes_no_value_error("no-signoff");
            }
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            value if value.starts_with("--quiet=") => {
                return commit_option_takes_no_value_error("quiet");
            }
            value if value.starts_with("--no-quiet=") => {
                return commit_option_takes_no_value_error("no-quiet");
            }
            "-a" | "--all" => all = true,
            "--no-all" => all = false,
            value if value.starts_with("--all=") => {
                return commit_option_takes_no_value_error("all");
            }
            value if value.starts_with("--no-all=") => {
                return commit_option_takes_no_value_error("no-all");
            }
            "--allow-empty" => allow_empty = true,
            "--no-allow-empty" => allow_empty = false,
            "--allow-empty-message" => allow_empty_message = true,
            "--no-allow-empty-message" => allow_empty_message = false,
            value if value.starts_with("--allow-empty=") => {
                return commit_option_takes_no_value_error("allow-empty");
            }
            value if value.starts_with("--no-allow-empty=") => {
                return commit_option_takes_no_value_error("no-allow-empty");
            }
            value if value.starts_with("--allow-empty-message=") => {
                return commit_option_takes_no_value_error("allow-empty-message");
            }
            value if value.starts_with("--no-allow-empty-message=") => {
                return commit_option_takes_no_value_error("no-allow-empty-message");
            }
            "--author" => {
                let Some(author) = iter.next() else {
                    return commit_author_requires_value_error();
                };
                author_override = Some(author.to_string());
            }
            value if value.starts_with("--author=") => {
                author_override = Some(value["--author=".len()..].to_string());
            }
            "--no-author" => author_override = None,
            value if value.starts_with("--no-author=") => {
                return commit_option_takes_no_value_error("no-author");
            }
            "--date" => {
                let Some(date) = iter.next() else {
                    return commit_date_requires_value_error();
                };
                author_date = Some(date.to_string());
            }
            value if value.starts_with("--date=") => {
                author_date = Some(value["--date=".len()..].to_string());
            }
            "--no-date" => author_date = None,
            value if value.starts_with("--no-date=") => {
                return commit_option_takes_no_value_error("no-date");
            }
            "-n" | "--no-verify" | "--verify" => {}
            value if value.starts_with("--no-verify=") => {
                return commit_option_takes_no_value_error("no-verify");
            }
            value if value.starts_with("--verify=") => {
                return commit_option_takes_no_value_error("no-no-verify");
            }
            "-S" | "--gpg-sign" => gpg_sign = true,
            value if value.starts_with("-S") && value.len() > 2 => {
                gpg_sign = true;
            }
            value if value.starts_with("--gpg-sign=") => {
                gpg_sign = true;
            }
            "--no-gpg-sign" => gpg_sign = false,
            value if value.starts_with("--no-gpg-sign=") => {
                return commit_option_takes_no_value_error("no-gpg-sign");
            }
            "--post-rewrite" | "--no-post-rewrite" => {}
            value if value.starts_with("--post-rewrite=") => {
                return commit_option_takes_no_value_error("no-no-post-rewrite");
            }
            value if value.starts_with("--no-post-rewrite=") => {
                return commit_option_takes_no_value_error("no-post-rewrite");
            }
            "--status" | "--no-status" => {}
            value if value.starts_with("--status=") => {
                return commit_option_takes_no_value_error("status");
            }
            value if value.starts_with("--no-status=") => {
                return commit_option_takes_no_value_error("no-status");
            }
            "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            value if value.starts_with("--dry-run=") => {
                return commit_option_takes_no_value_error("dry-run");
            }
            value if value.starts_with("--no-dry-run=") => {
                return commit_option_takes_no_value_error("no-dry-run");
            }
            "--short" => {
                status_mode = CommitStatusMode::Short;
                null_implied_status = false;
            }
            "--no-short" => {
                if status_mode == CommitStatusMode::Short {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--short=") => {
                return commit_option_takes_no_value_error("short");
            }
            value if value.starts_with("--no-short=") => {
                return commit_option_takes_no_value_error("no-short");
            }
            "--porcelain" => {
                status_mode = CommitStatusMode::Porcelain;
                null_implied_status = false;
            }
            "--no-porcelain" => {
                if status_mode == CommitStatusMode::Porcelain {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--porcelain=") => {
                return commit_option_takes_no_value_error("porcelain");
            }
            value if value.starts_with("--no-porcelain=") => {
                return commit_option_takes_no_value_error("no-porcelain");
            }
            "-z" | "--null" => {
                if status_mode == CommitStatusMode::Normal {
                    status_mode = CommitStatusMode::Short;
                    null_implied_status = true;
                }
                status_null = true;
            }
            "--no-null" => {
                status_null = false;
                if null_implied_status {
                    status_mode = CommitStatusMode::Normal;
                    null_implied_status = false;
                }
            }
            value if value.starts_with("--null=") => {
                return commit_option_takes_no_value_error("null");
            }
            value if value.starts_with("--no-null=") => {
                return commit_option_takes_no_value_error("no-null");
            }
            "--long" => {
                status_mode = CommitStatusMode::Long;
                null_implied_status = false;
            }
            "--no-long" => {
                if status_mode == CommitStatusMode::Long {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--long=") => {
                return commit_option_takes_no_value_error("long");
            }
            value if value.starts_with("--no-long=") => {
                return commit_option_takes_no_value_error("no-long");
            }
            "--ahead-behind" | "--no-ahead-behind" => {}
            value if value.starts_with("--ahead-behind=") => {
                return commit_option_takes_no_value_error("ahead-behind");
            }
            value if value.starts_with("--no-ahead-behind=") => {
                return commit_option_takes_no_value_error("no-ahead-behind");
            }
            "--interactive" => interactive = true,
            "--no-interactive" => interactive = false,
            value if value.starts_with("--interactive=") => {
                return commit_option_takes_no_value_error("interactive");
            }
            value if value.starts_with("--no-interactive=") => {
                return commit_option_takes_no_value_error("no-interactive");
            }
            "-p" | "--patch" => patch = true,
            "--no-patch" => patch = false,
            value if value.starts_with("--patch=") => {
                return commit_option_takes_no_value_error("patch");
            }
            value if value.starts_with("--no-patch=") => {
                return commit_option_takes_no_value_error("no-patch");
            }
            "-U" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(true);
                };
                commit_validate_unified_context(value, true)?;
                unified_context = true;
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                commit_validate_unified_context(&value[2..], true)?;
                unified_context = true;
            }
            "--unified" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(false);
                };
                commit_validate_unified_context(value, false)?;
                unified_context = true;
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                commit_validate_unified_context(&value["--unified=".len()..], false)?;
                unified_context = true;
            }
            "--inter-hunk-context" => {
                let Some(value) = iter.next() else {
                    return commit_inter_hunk_context_requires_value_error();
                };
                commit_validate_inter_hunk_context(value)?;
                inter_hunk_context = true;
            }
            "--inter-hunk-context=" => {
                return commit_inter_hunk_context_expects_numerical_value_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                commit_validate_inter_hunk_context(&value["--inter-hunk-context=".len()..])?;
                inter_hunk_context = true;
            }
            "-v" | "--verbose" | "--no-verbose" => {}
            value if value.starts_with("--verbose=") => {
                return commit_option_takes_no_value_error("verbose");
            }
            value if value.starts_with("--no-verbose=") => {
                return commit_option_takes_no_value_error("no-verbose");
            }
            "-u" | "-uno" | "-unormal" | "-uall" | "--untracked-files" => {}
            value if value.starts_with("-u") && value.len() > 2 => {
                return commit_invalid_untracked_files_mode_error(&value[2..]);
            }
            value if value.starts_with("--untracked-files=") => {
                let mode = &value["--untracked-files=".len()..];
                match mode {
                    "no" | "normal" | "all" => {}
                    _ => return commit_invalid_untracked_files_mode_error(mode),
                }
            }
            "--no-untracked-files" => {}
            value if value.starts_with("--no-untracked-files=") => {
                return commit_option_takes_no_value_error("no-untracked-files");
            }
            "--pathspec-from-file" => {
                let Some(value) = iter.next() else {
                    return commit_pathspec_from_file_requires_value_error();
                };
                pathspec_from_file = Some(value.to_string());
                pathspec_from_file_active = true;
            }
            value if value.starts_with("--pathspec-from-file=") => {
                pathspec_from_file = Some(value["--pathspec-from-file=".len()..].to_string());
                pathspec_from_file_active = true;
            }
            "--no-pathspec-from-file" => pathspec_from_file_active = false,
            value if value.starts_with("--no-pathspec-from-file=") => {
                return commit_option_takes_no_value_error("no-pathspec-from-file");
            }
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            value if value.starts_with("--pathspec-file-nul=") => {
                return commit_option_takes_no_value_error("pathspec-file-nul");
            }
            value if value.starts_with("--no-pathspec-file-nul=") => {
                return commit_option_takes_no_value_error("no-pathspec-file-nul");
            }
            "-i" | "--include" => include_without_paths = true,
            "--no-include" => include_without_paths = false,
            value if value.starts_with("--include=") => {
                return commit_option_takes_no_value_error("include");
            }
            value if value.starts_with("--no-include=") => {
                return commit_option_takes_no_value_error("no-include");
            }
            "-o" | "--only" => only_without_paths = true,
            "--no-only" => only_without_paths = false,
            value if value.starts_with("--only=") => {
                return commit_option_takes_no_value_error("only");
            }
            value if value.starts_with("--no-only=") => {
                return commit_option_takes_no_value_error("no-only");
            }
            "-e" | "--edit" => edit_flag = Some(true),
            "--no-edit" => edit_flag = Some(false),
            value if value.starts_with("--edit=") => {
                return commit_option_takes_no_value_error("edit");
            }
            value if value.starts_with("--no-edit=") => {
                return commit_option_takes_no_value_error("no-edit");
            }
            "--branch" | "--no-branch" => {}
            value if value.starts_with("--branch=") => {
                return commit_option_takes_no_value_error("branch");
            }
            value if value.starts_with("--no-branch=") => {
                return commit_option_takes_no_value_error("no-branch");
            }
            "-t" => {
                let Some(_template) = iter.next() else {
                    return commit_template_short_requires_value_error();
                };
            }
            value if value.starts_with("-t") && value.len() > 2 => {}
            "--template" => {
                let Some(_template) = iter.next() else {
                    return commit_template_requires_value_error();
                };
            }
            value if value.starts_with("--template=") => {}
            "--no-template" => {}
            value if value.starts_with("--no-template=") => {
                return commit_option_takes_no_value_error("no-template");
            }
            "--cleanup" => {
                let Some(value) = iter.next() else {
                    return commit_cleanup_requires_value_error();
                };
                cleanup_mode = Some(parse_commit_cleanup_mode(value)?);
            }
            value if value.starts_with("--cleanup=") => {
                cleanup_mode = Some(parse_commit_cleanup_mode(&value["--cleanup=".len()..])?);
            }
            "--no-cleanup" => cleanup_mode = Some(CommitCleanupMode::Whitespace),
            value if value.starts_with("--no-cleanup=") => {
                return commit_option_takes_no_value_error("no-cleanup");
            }
            "--" => {
                if pathspec_from_file_active && !iter.as_slice().is_empty() {
                    return commit_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspec_args.extend(iter.by_ref().cloned());
            }
            value => {
                if value.starts_with('-') {
                    if pathspec_from_file_active {
                        return commit_pathspec_from_file_with_inline_pathspec_error();
                    }
                    return Err(GitError::Command(format!(
                        "unsupported commit argument {value}; currently supports -m and -F"
                    )));
                }
                if pathspec_from_file_active {
                    return commit_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspec_args.push(value.to_string());
            }
        }
    }
    if reuse_message.is_some() && !message_chunks.is_empty() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '-m' and '{option}' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if reuse_message.is_some() && file_message.is_some() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '{option}' and '-F' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if fixup_commit.is_some() && reuse_message.is_some() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '{option}' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if let Some(fixup) = &fixup_commit
        && fixup.is_amend_style()
        && !message_chunks.is_empty()
    {
        let option = if fixup.is_reword() {
            "--fixup:reword"
        } else {
            "--fixup:amend"
        };
        eprintln!("fatal: options '-m' and '{option}' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if squash_commit.is_some() && fixup_commit.is_some() {
        eprintln!("fatal: options '--squash' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if fixup_commit.is_some() && file_message.is_some() {
        eprintln!("fatal: options '-F' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if reset_author && reuse_message.is_none() && !amend {
        eprintln!("fatal: --reset-author can be used only with -C, -c or --amend.");
        return Err(GitError::Exit(128));
    }
    if file_message.is_some() && !message_chunks.is_empty() {
        eprintln!("fatal: options '-m' and '-F' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if include_without_paths || only_without_paths {
        eprintln!("fatal: No paths with --include/--only does not make sense.");
        return Err(GitError::Exit(128));
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file.as_deref() {
        let pathspecs =
            read_commit_pathspecs_from_file(Path::new(pathspec_file), pathspec_file_nul)?;
        if pathspec_from_file_active {
            pathspec_args.extend(
                pathspecs
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned()),
            );
        }
    }
    if unified_context && !interactive && !patch {
        eprintln!("fatal: the option '--unified' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if inter_hunk_context && !interactive && !patch {
        eprintln!("fatal: the option '--inter-hunk-context' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if status_mode != CommitStatusMode::Normal {
        return cmd_commit_status_preview(status_mode, status_null);
    }
    if dry_run {
        return cmd_commit_long_status_preview();
    }
    if gpg_sign {
        return Err(GitError::Unsupported(
            "commit gpg signing is not implemented".into(),
        ));
    }
    if interactive || patch {
        return Err(GitError::Unsupported(
            "commit interactive patch selection is not implemented".into(),
        ));
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let in_merge = git_dir.join("MERGE_HEAD").is_file();
    let in_cherry_pick = git_dir.join("CHERRY_PICK_HEAD").is_file();
    let in_revert = git_dir.join("REVERT_HEAD").is_file();
    if !pathspec_args.is_empty() {
        if in_merge {
            eprintln!("fatal: cannot do a partial commit during a merge.");
            return Err(GitError::Exit(128));
        }
        if in_cherry_pick || in_revert {
            eprintln!("fatal: cannot do a partial commit during a cherry-pick.");
            return Err(GitError::Exit(128));
        }
    }
    if amend {
        if in_merge {
            eprintln!("fatal: You are in the middle of a merge -- cannot amend.");
            return Err(GitError::Exit(128));
        }
        if in_cherry_pick || in_revert {
            eprintln!("fatal: You are in the middle of a cherry-pick -- cannot amend.");
            return Err(GitError::Exit(128));
        }
    }
    // `i18n.commitEncoding` is recorded as the commit's `encoding` header so that
    // `git log` can re-encode the message to the log output encoding (UTF-8 by
    // default). git omits the header for UTF-8.
    let commit_encoding_header = read_repo_config(&git_dir)
        .ok()
        .and_then(|config| {
            config
                .get("i18n", None, "commitEncoding")
                .map(str::to_string)
        })
        .filter(|enc| !encoding_is_utf8(enc))
        .map(String::into_bytes);
    let committer = commit_identity_from_env("COMMITTER")?;
    let amended_commit = amend
        .then(|| read_amended_commit(&git_dir, format))
        .transpose()?;
    let reused_commit = reuse_message
        .as_deref()
        .map(|rev| read_reused_commit(&git_dir, format, rev))
        .transpose()?;
    let fixup_message = fixup_commit
        .as_ref()
        .map(|fixup| read_fixup_commit_message(&git_dir, format, fixup))
        .transpose()?;
    let fixup_reword_tree = if fixup_commit.as_ref().is_some_and(CommitFixup::is_reword) {
        let Some(commit) = read_head_commit(&git_dir, format)? else {
            eprintln!("fatal: You have nothing to amend.");
            return Err(GitError::Exit(128));
        };
        Some(commit.tree)
    } else {
        None
    };
    let squash_message = squash_commit
        .as_deref()
        .map(|rev| read_squash_commit_message(&git_dir, format, rev))
        .transpose()?;
    let author = if reset_author {
        build_commit_author_identity(author_override.as_deref(), author_date.as_deref())?
    } else if let Some(commit) = &reused_commit {
        build_reused_commit_author_identity(
            &commit.author,
            author_override.as_deref(),
            author_date.as_deref(),
        )?
    } else if let Some(commit) = &amended_commit {
        build_reused_commit_author_identity(
            &commit.author,
            author_override.as_deref(),
            author_date.as_deref(),
        )?
    } else {
        build_commit_author_identity(author_override.as_deref(), author_date.as_deref())?
    };
    let had_file_message = file_message.is_some();
    let message = reused_commit
        .as_ref()
        .map(|commit| {
            if let Some(squash_message) = &squash_message {
                commit_squash_message(squash_message, Some(&commit.message), None, &[])
            } else {
                commit.message.clone()
            }
        })
        .or_else(|| {
            squash_message.as_ref().map(|message| {
                commit_squash_message(message, None, file_message.as_deref(), &message_chunks)
            })
        })
        .or_else(|| {
            fixup_message.as_ref().map(|message| {
                commit_fixup_message(message, file_message.as_deref(), &message_chunks)
            })
        })
        .or_else(|| {
            if amend && file_message.is_none() && message_chunks.is_empty() {
                amended_commit.as_ref().map(|commit| commit.message.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            if (in_merge || in_cherry_pick || in_revert)
                && file_message.is_none()
                && message_chunks.is_empty()
                && reuse_message.is_none()
                && fixup_commit.is_none()
                && squash_commit.is_none()
            {
                if in_merge {
                    read_merge_message_from_file(&git_dir).ok()
                } else {
                    // Keep the commented "# Conflicts:" block intact: the
                    // editor template shows it and the post-editor cleanup
                    // strips it.
                    fs::read(git_dir.join("MERGE_MSG")).ok()
                }
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            file_message.unwrap_or_else(|| commit_message_from_prepared_chunks(&message_chunks))
        });
    if all {
        commit_stage_tracked_changes(&git_dir, format)?;
    }
    let mut message = if signoff {
        commands::replay::append_signoff_before_comments(message, &commit_signoff_from_env()?)
    } else {
        message
    };
    // Editor flow: a commit without an explicit message source launches the
    // editor over COMMIT_EDITMSG (the in-merge / rebase conclude paths keep
    // their historical no-editor behavior).
    let had_message_source = had_file_message
        || !message_chunks.is_empty()
        || reuse_message.is_some()
        || fixup_commit.is_some()
        || squash_commit.is_some();
    let in_rebase = rebase_in_progress(&git_dir);
    let use_editor = !in_rebase
        && !in_merge
        && (edit_flag == Some(true) || (edit_flag != Some(false) && !had_message_source));
    if use_editor {
        let editmsg = git_dir.join("COMMIT_EDITMSG");
        fs::write(&editmsg, &message)?;
        if let Err(err) = commands::replay::launch_editor(&git_dir, &editmsg) {
            eprintln!("error: {err}");
            eprintln!("Please supply the message using either -m or -F option.");
            return Err(GitError::Exit(1));
        }
        message = fs::read(&editmsg)?;
        if cleanup_mode.is_none() {
            message = commands::replay::strip_comment_lines(
                &message,
                commands::replay::comment_char(&git_dir),
            );
        }
    }
    if let Some(cleanup_mode) = cleanup_mode {
        message = commit_cleanup_message(message, cleanup_mode);
    }
    let message_with_trailers =
        commands::tag::tag_message_with_trailers(message.clone(), &trailers);
    if (in_cherry_pick || in_revert)
        && !allow_empty_message
        && commit_message_is_empty(&message_with_trailers)
    {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    let message = message;
    let message = tag_message_with_trailers(message, &trailers);
    if in_rebase {
        return conclude_rebase_step_via_commit(
            &git_dir, format, author, committer, message, quiet,
        );
    }
    if in_merge {
        return conclude_in_progress_merge(&git_dir, format, message, quiet);
    }
    if in_cherry_pick || in_revert {
        return conclude_replay_via_commit(
            &git_dir,
            format,
            message,
            allow_empty,
            allow_empty_message,
            author,
            author_override.is_none() && !reset_author,
            quiet,
        );
    }
    if !allow_empty_message && commit_message_is_empty(&message) {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    if !pathspec_args.is_empty() {
        return commit_partial_paths(
            &git_dir,
            format,
            &pathspec_args,
            author,
            committer,
            message,
            quiet,
        );
    }
    if !allow_empty
        && !amend
        && fixup_reword_tree.is_none()
        && commit_index_matches_head(&git_dir, format)?
    {
        print_clean_commit_status(&git_dir, format)?;
        return Err(GitError::Exit(1));
    }
    let options = sley_sequencer::CommitIndexOptions {
        author,
        committer,
        reflog_message: commit_reflog_message(&message, amend),
        message,
        encoding: commit_encoding_header,
    };
    let result = if amend {
        sley_sequencer::amend_index(&git_dir, format, options)
    } else if let Some(tree) = fixup_reword_tree {
        sley_sequencer::commit_tree_at_head(&git_dir, format, tree, options)
    } else {
        sley_sequencer::commit_index(&git_dir, format, options)
    }?;
    if !quiet {
        println!("{}", result.oid);
    }
    Ok(())
}

/// Conclude an in-progress cherry-pick / revert via `git commit`: commit the
/// staged resolution with the picked commit's authorship, then run the
/// sequencer post-commit cleanup (CHERRY_PICK_HEAD / REVERT_HEAD removal and
/// the last-pick sequencer-state teardown).
#[allow(clippy::too_many_arguments)]
fn conclude_replay_via_commit(
    git_dir: &Path,
    format: ObjectFormat,
    message: Vec<u8>,
    allow_empty: bool,
    allow_empty_message: bool,
    env_author: Vec<u8>,
    use_pick_author: bool,
    quiet: bool,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    let head = commands::merge_rebase::head_commit_oid(&refs)?;
    let index_path = sley_worktree::repository_index_path(git_dir);
    if let Ok(bytes) = fs::read(&index_path) {
        let index = Index::parse(&bytes, format)?;
        let unmerged: BTreeSet<String> = index
            .entries
            .iter()
            .filter(|entry| index_entry_stage(entry) > 0)
            .map(|entry| entry.path.to_string())
            .collect();
        if !unmerged.is_empty() {
            for path in &unmerged {
                println!("U\t{path}");
            }
            eprintln!("error: Committing is not possible because you have unmerged files.");
            eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
            eprintln!("hint: as appropriate to mark resolution and make a commit.");
            eprintln!("fatal: Exiting because of an unresolved conflict.");
            return Err(GitError::Exit(128));
        }
    }
    let head_tree = match &head {
        Some(oid) => commands::merge_rebase::commit_tree_oid(&db, format, oid)?,
        None => ObjectId::empty_tree(format),
    };
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    let cherry_pick_head = git_dir.join("CHERRY_PICK_HEAD");
    if !allow_empty && tree == head_tree {
        let action = if cherry_pick_head.is_file() {
            "cherry-pick"
        } else {
            "revert"
        };
        eprintln!(
            "The previous cherry-pick is now empty, possibly due to conflict resolution."
        );
        eprintln!("If you wish to commit it anyway, use:");
        eprintln!();
        eprintln!("    git commit --allow-empty");
        eprintln!();
        eprintln!("Otherwise, please use 'git {action} --skip'");
        return Err(GitError::Exit(1));
    }
    if !allow_empty_message && commit_message_is_empty(&message) {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    let author = if use_pick_author && cherry_pick_head.is_file() {
        let text = fs::read_to_string(&cherry_pick_head)?;
        let oid = ObjectId::from_hex(format, text.trim())?;
        let object = db.read_object(&oid)?;
        Commit::parse(format, &object.body)?.author
    } else {
        env_author
    };
    let committer = commit_identity_from_env("COMMITTER")?;
    let new_oid = sley_sequencer::create_commit(
        &mut FileObjectDatabase::from_git_dir(git_dir, format),
        sley_sequencer::CommitCreate {
            tree,
            parents: head.iter().copied().collect(),
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head.unwrap_or_else(|| ObjectId::null(format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(RefTarget::Direct),
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: commit_reflog_message(&message, false),
        }),
    });
    tx.commit()?;
    sley_sequencer::replay::post_commit_cleanup(git_dir);
    for name in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE", "SQUASH_MSG"] {
        let _ = fs::remove_file(git_dir.join(name));
    }
    if !quiet {
        println!("{new_oid}");
    }
    Ok(())
}

/// Partial commit (`git commit [-m ...] -- <paths>`): stage the named paths'
/// working-tree contents (clean filters applied, directories expanded over
/// the tracked entries beneath them), then record HEAD's tree with just those
/// paths replaced. Mirrors git's `--only` default for tracked-file usage.
fn commit_partial_paths(
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[String],
    author: Vec<u8>,
    committer: Vec<u8>,
    message: Vec<u8>,
    quiet: bool,
) -> Result<()> {
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let cwd = env::current_dir()?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    let head = commands::merge_rebase::head_commit_oid(&refs)?;
    let mut tree_map = match &head {
        Some(oid) => {
            let tree = commands::merge_rebase::commit_tree_oid(&db, format, oid)?;
            stash_tree_entry_map(&db, format, &tree)?
        }
        None => BTreeMap::new(),
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
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
    let known: BTreeSet<Vec<u8>> = index
        .entries
        .iter()
        .map(|entry| entry.path.clone().into_bytes())
        .chain(tree_map.keys().cloned())
        .collect();

    // Expand the pathspecs over the tracked entries (directories and `.`
    // cover everything beneath them).
    let mut rel_paths: Vec<Vec<u8>> = Vec::new();
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    for path in paths {
        let absolute = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            cwd.join(path)
        };
        let rel: Vec<u8> = match absolute.strip_prefix(&worktree_root) {
            Ok(stripped) => stripped.to_string_lossy().into_owned().into_bytes(),
            Err(_) => {
                return Err(GitError::InvalidPath(format!(
                    "pathspec outside repository: {path}"
                )));
            }
        };
        let mut matched = false;
        if rel.is_empty() {
            // `.` at the worktree root: every tracked entry.
            for tracked in &known {
                if seen.insert(tracked.clone()) {
                    rel_paths.push(tracked.clone());
                }
                matched = true;
            }
        } else if known.contains(&rel) {
            if seen.insert(rel.clone()) {
                rel_paths.push(rel.clone());
            }
            matched = true;
        } else {
            let mut prefix = rel.clone();
            prefix.push(b'/');
            for tracked in &known {
                if tracked.starts_with(&prefix) {
                    if seen.insert(tracked.clone()) {
                        rel_paths.push(tracked.clone());
                    }
                    matched = true;
                }
            }
        }
        if !matched {
            eprintln!("error: pathspec '{path}' did not match any file(s) known to git");
            return Err(GitError::Exit(128));
        }
    }

    // Stage the matched paths with the regular add machinery (clean filters,
    // mode bits) — partial commits update those index entries too.
    let config = read_repo_config(git_dir)?;
    let ordered: Vec<sley_worktree::UpdateIndexPath> = rel_paths
        .iter()
        .map(|rel| sley_worktree::UpdateIndexPath {
            path: worktree_root.join(String::from_utf8_lossy(rel).as_ref()),
            chmod: None,
        })
        .collect();
    sley_worktree::update_index_ordered_paths_filtered(
        &worktree_root,
        git_dir,
        format,
        &ordered,
        sley_worktree::UpdateIndexOptions {
            add: true,
            remove: true,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
        },
        &config,
        false,
    )?;

    // Overlay the staged state of the matched paths onto HEAD's tree.
    let updated_index = Index::parse(&fs::read(&index_path)?, format)?;
    let staged: BTreeMap<Vec<u8>, (u32, ObjectId)> = updated_index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .map(|entry| (entry.path.clone().into_bytes(), (entry.mode, entry.oid)))
        .collect();
    for rel in &rel_paths {
        match staged.get(rel) {
            Some(entry) => {
                tree_map.insert(rel.clone(), *entry);
            }
            None => {
                tree_map.remove(rel);
            }
        }
    }
    let tree = write_tree_from_entry_map(&db, format, &tree_map)?;
    let new_oid = sley_sequencer::create_commit(
        &mut FileObjectDatabase::from_git_dir(git_dir, format),
        sley_sequencer::CommitCreate {
            tree,
            parents: head.iter().copied().collect(),
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head.unwrap_or_else(|| ObjectId::null(format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(RefTarget::Direct),
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: commit_reflog_message(&message, false),
        }),
    });
    tx.commit()?;
    sley_sequencer::replay::post_commit_cleanup(git_dir);
    if !quiet {
        println!("{new_oid}");
    }
    Ok(())
}

/// Write a tree object hierarchy from a flat `path -> (mode, oid)` map
/// (grouping by leading path component, mirroring fast-import's writer).
fn write_tree_from_entry_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
) -> Result<ObjectId> {
    let _ = format;
    write_entry_map_level(db, entries, &[])
}

fn write_entry_map_level(
    db: &FileObjectDatabase,
    entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    prefix: &[u8],
) -> Result<ObjectId> {
    let mut tree_entries: Vec<sley_object::TreeEntry> = Vec::new();
    let mut subdirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    let prefix_len = if prefix.is_empty() { 0 } else { prefix.len() + 1 };
    for (path, (mode, oid)) in entries {
        if !prefix.is_empty()
            && (!path.starts_with(prefix) || path.get(prefix.len()) != Some(&b'/'))
        {
            continue;
        }
        let rel = &path[prefix_len..];
        if let Some(slash) = rel.iter().position(|b| *b == b'/') {
            subdirs.insert(rel[..slash].to_vec());
        } else {
            tree_entries.push(sley_object::TreeEntry {
                mode: *mode,
                name: BString::from(rel.to_vec()),
                oid: *oid,
            });
        }
    }
    for dir in subdirs {
        let mut sub_prefix = prefix.to_vec();
        if !sub_prefix.is_empty() {
            sub_prefix.push(b'/');
        }
        sub_prefix.extend_from_slice(&dir);
        let sub_oid = write_entry_map_level(db, entries, &sub_prefix)?;
        tree_entries.push(sley_object::TreeEntry {
            mode: 0o040000,
            name: BString::from(dir),
            oid: sub_oid,
        });
    }
    // Tree entries collate with subtrees as though their name ends in `/`.
    tree_entries.sort_by_key(|entry| {
        let mut key = entry.name.clone().into_bytes();
        if entry.mode == 0o040000 {
            key.push(b'/');
        }
        key
    });
    db.write_object(EncodedObject::new(
        ObjectType::Tree,
        sley_object::Tree {
            entries: tree_entries,
        }
        .write(),
    ))
}

enum CommitFixup {
    Plain(String),
    Amend { rev: String, reword: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitStatusMode {
    Normal,
    Short,
    Porcelain,
    Long,
}

fn cmd_commit_status_preview(mode: CommitStatusMode, null: bool) -> Result<()> {
    let mut args = Vec::new();
    match mode {
        CommitStatusMode::Normal => {}
        CommitStatusMode::Short => args.push("--short".to_string()),
        CommitStatusMode::Porcelain => args.push("--porcelain".to_string()),
        CommitStatusMode::Long => return cmd_commit_long_status_preview(),
    }
    if null {
        args.push("-z".to_string());
    }
    cmd_status(&args)
}

fn cmd_commit_long_status_preview() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let entries = sley_worktree::short_status_with_options(
        &worktree_root,
        &git_dir,
        format,
        sley_worktree::ShortStatusOptions {
            include_ignored: false,
            untracked_mode: sley_worktree::StatusUntrackedMode::Normal,
        },
    )?;
    let committable = status_entries_have_index_changes(&entries);
    print_status_long(&git_dir, format, entries, true, false, true)?;
    if committable {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

impl CommitFixup {
    fn parse(value: &str) -> Result<Self> {
        if let Some(rev) = value.strip_prefix("amend:") {
            Ok(Self::Amend {
                rev: rev.to_string(),
                reword: false,
            })
        } else if let Some(rev) = value.strip_prefix("reword:") {
            Ok(Self::Amend {
                rev: rev.to_string(),
                reword: true,
            })
        } else if value.contains(':')
            && value
                .split_once(':')
                .is_some_and(|(mode, _)| !mode.is_empty())
        {
            eprintln!("fatal: unknown option: --fixup={value}");
            Err(GitError::Exit(128))
        } else {
            Ok(Self::Plain(value.to_string()))
        }
    }

    fn rev(&self) -> &str {
        match self {
            Self::Plain(rev) | Self::Amend { rev, .. } => rev,
        }
    }

    fn is_amend_style(&self) -> bool {
        matches!(self, Self::Amend { .. })
    }

    fn is_reword(&self) -> bool {
        matches!(self, Self::Amend { reword: true, .. })
    }
}

fn commit_author_requires_value_error() -> Result<()> {
    eprintln!("error: option `author' requires a value");
    Err(GitError::Exit(129))
}

fn commit_date_requires_value_error() -> Result<()> {
    eprintln!("error: option `date' requires a value");
    Err(GitError::Exit(129))
}

fn commit_cleanup_requires_value_error() -> Result<()> {
    eprintln!("error: option `cleanup' requires a value");
    Err(GitError::Exit(129))
}

fn commit_template_requires_value_error() -> Result<()> {
    eprintln!("error: option `template' requires a value");
    Err(GitError::Exit(129))
}

fn commit_template_short_requires_value_error() -> Result<()> {
    eprintln!("error: switch `t' requires a value");
    Err(GitError::Exit(129))
}

fn commit_reuse_message_requires_value_error(short: bool, reedit: bool) -> Result<()> {
    if short {
        let switch = if reedit { "c" } else { "C" };
        eprintln!("error: switch `{switch}' requires a value");
    } else {
        let option = if reedit {
            "reedit-message"
        } else {
            "reuse-message"
        };
        eprintln!("error: option `{option}' requires a value");
    }
    Err(GitError::Exit(129))
}

fn commit_fixup_requires_value_error() -> Result<()> {
    eprintln!("error: option `fixup' requires a value");
    Err(GitError::Exit(129))
}

fn commit_squash_requires_value_error() -> Result<()> {
    eprintln!("error: option `squash' requires a value");
    Err(GitError::Exit(129))
}

fn commit_trailer_requires_value_error() -> Result<()> {
    eprintln!("error: option `trailer' requires a value");
    Err(GitError::Exit(129))
}

fn commit_pathspec_from_file_requires_value_error() -> Result<()> {
    eprintln!("error: option `pathspec-from-file' requires a value");
    Err(GitError::Exit(129))
}

fn commit_pathspec_from_file_with_inline_pathspec_error() -> Result<()> {
    eprintln!("fatal: '--pathspec-from-file' and pathspec arguments cannot be used together");
    Err(GitError::Exit(128))
}

fn commit_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn commit_invalid_untracked_files_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid untracked files mode '{mode}'");
    Err(GitError::Exit(128))
}

fn read_porcelain_commit_message_file(path: &str) -> Result<Vec<u8>> {
    let mut message = read_commit_message_file(path)?;
    if !message.is_empty() && !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    Ok(message)
}

fn commit_message_is_empty(message: &[u8]) -> bool {
    message.iter().all(u8::is_ascii_whitespace)
}

fn parse_commit_cleanup_mode(value: &str) -> Result<CommitCleanupMode> {
    match value {
        "strip" => Ok(CommitCleanupMode::Strip),
        "whitespace" | "scissors" | "default" => Ok(CommitCleanupMode::Whitespace),
        "verbatim" => Ok(CommitCleanupMode::Verbatim),
        _ => {
            eprintln!("fatal: Invalid cleanup mode {value}");
            Err(GitError::Exit(128))
        }
    }
}

fn read_fixup_commit_message(
    git_dir: &Path,
    format: ObjectFormat,
    fixup: &CommitFixup,
) -> Result<Vec<u8>> {
    let commit = read_reused_commit(git_dir, format, fixup.rev())?;
    let subject = commit_subject(&commit.message);
    match fixup {
        CommitFixup::Plain(_) => Ok(format!("fixup! {subject}\n").into_bytes()),
        CommitFixup::Amend { .. } => {
            let mut message = format!("amend! {subject}\n\n").into_bytes();
            message.extend_from_slice(&commit.message);
            Ok(message)
        }
    }
}

fn read_squash_commit_message(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<Vec<u8>> {
    let commit = read_reused_commit(git_dir, format, rev)?;
    Ok(format!("squash! {}\n", commit_subject(&commit.message)).into_bytes())
}

fn commit_fixup_message(
    fixup_message: &[u8],
    file_message: Option<&[u8]>,
    message_chunks: &[Vec<u8>],
) -> Vec<u8> {
    let body = file_message
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| commit_message_from_prepared_chunks(message_chunks));
    if body.is_empty() {
        return fixup_message.to_vec();
    }
    let mut message = fixup_message.to_vec();
    if !message.ends_with(b"\n\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(&body);
    message
}

fn commit_squash_message(
    squash_message: &[u8],
    reused_message: Option<&[u8]>,
    file_message: Option<&[u8]>,
    message_chunks: &[Vec<u8>],
) -> Vec<u8> {
    let body = reused_message
        .map(commit_message_body)
        .or_else(|| file_message.map(<[u8]>::to_vec))
        .unwrap_or_else(|| commit_message_from_prepared_chunks(message_chunks));
    if body.is_empty() {
        return squash_message.to_vec();
    }
    let mut message = squash_message.to_vec();
    if !message.ends_with(b"\n\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(&body);
    message
}

fn commit_message_body(message: &[u8]) -> Vec<u8> {
    let Some(first_lf) = message.iter().position(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let body_start = if message.get(first_lf + 1) == Some(&b'\n') {
        first_lf + 2
    } else {
        first_lf + 1
    };
    message[body_start..].to_vec()
}

fn read_amended_commit(git_dir: &Path, format: ObjectFormat) -> Result<Commit> {
    match read_head_commit(git_dir, format)? {
        Some(commit) => Ok(commit),
        None => {
            eprintln!("fatal: You have nothing to amend.");
            Err(GitError::Exit(128))
        }
    }
}

fn read_head_commit(git_dir: &Path, format: ObjectFormat) -> Result<Option<Commit>> {
    let store = FileRefStore::new(git_dir, format);
    let head = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => store.read_ref(&name)?,
        direct => direct,
    };
    let Some(RefTarget::Direct(oid)) = head else {
        return Ok(None);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            oid,
            object.object_type.as_str()
        )));
    }
    Commit::parse(format, &object.body).map(Some)
}

/// First line of the commit message at `oid`, for the `HEAD is now at <oid>
/// <subject>` line a detached-HEAD checkout prints. Best-effort: an unreadable or
/// non-commit object yields an empty subject (git still prints the abbreviated
/// oid).
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

fn build_reused_commit_author_identity(
    reused_author: &[u8],
    author: Option<&str>,
    date: Option<&str>,
) -> Result<Vec<u8>> {
    if author.is_none() && date.is_none() {
        return Ok(reused_author.to_vec());
    }
    let (reused_name, reused_email, reused_date) = parse_commit_identity_parts(reused_author)?;
    let (name, email) = if let Some(author) = author {
        parse_commit_author(author)?
    } else {
        (reused_name, reused_email)
    };
    // A `--date` override is raw user input; canonicalize it. The reused date
    // is already in canonical `<seconds> <tz>` form.
    let date = match date {
        Some(date) => canonicalize_commit_date(date),
        None => reused_date,
    };
    sley_sequencer::format_commit_identity(&name, &email, &date)
}

fn parse_commit_identity_parts(identity: &[u8]) -> Result<(String, String, String)> {
    let identity = std::str::from_utf8(identity)
        .map_err(|err| GitError::InvalidObject(format!("invalid commit identity: {err}")))?;
    let Some((left, timezone)) = identity.rsplit_once(' ') else {
        return Err(GitError::InvalidObject(
            "commit identity missing timezone".into(),
        ));
    };
    let Some((author, timestamp)) = left.rsplit_once(' ') else {
        return Err(GitError::InvalidObject(
            "commit identity missing timestamp".into(),
        ));
    };
    let (name, email) = parse_commit_author(author)?;
    Ok((name, email, format!("{timestamp} {timezone}")))
}

fn commit_stage_tracked_changes(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    let cwd = env::current_dir()?;
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let actions = resolve_add_update_actions(
        &cwd,
        &worktree_root,
        git_dir,
        format,
        Vec::new(),
        false,
        false,
    )?;
    let action_paths = actions
        .iter()
        .map(AddAction::path)
        .cloned()
        .collect::<Vec<_>>();
    if action_paths.is_empty() {
        return Ok(());
    }
    let config = read_repo_config(git_dir)?;
    sley_worktree::update_index_paths_filtered(
        &worktree_root,
        git_dir,
        format,
        &action_paths,
        sley_worktree::UpdateIndexOptions {
            add: true,
            remove: true,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
        },
        &config,
    )?;
    Ok(())
}

fn commit_index_matches_head(git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    let store = FileRefStore::new(git_dir, format);
    let head = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => store.read_ref(&name)?,
        direct => direct,
    };
    let Some(RefTarget::Direct(parent)) = head else {
        return Ok(false);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&parent)?;
    if object.object_type != ObjectType::Commit {
        return Ok(false);
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    Ok(commit.tree == tree)
}

fn print_clean_commit_status(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    if let Some(RefTarget::Symbolic(target)) = store.read_ref("HEAD")?
        && let Some(branch) = target.strip_prefix("refs/heads/")
    {
        println!("On branch {branch}");
    }
    println!("nothing to commit, working tree clean");
    Ok(())
}

pub(crate) fn cmd_status(args: &[String]) -> Result<()> {
    let mut short = false;
    let mut porcelain_v1 = false;
    let mut porcelain_v2 = false;
    let mut z = false;
    let mut explicit_long = false;
    let mut branch = false;
    let mut untracked_mode = sley_worktree::StatusUntrackedMode::Normal;
    let mut show_ignored = false;
    let mut show_stash = false;
    let mut ahead_behind = true;
    let mut path_args = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            path_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--short" | "-s" => {
                short = true;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--porcelain" | "--porcelain=1" | "--porcelain=v1" => {
                short = true;
                porcelain_v1 = true;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--porcelain=v2" | "--porcelain=2" => {
                short = true;
                porcelain_v1 = false;
                porcelain_v2 = true;
                explicit_long = false;
            }
            "--no-porcelain" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--branch" | "-b" => {
                short = true;
                branch = true;
                explicit_long = false;
            }
            "-sb" | "-bs" => {
                short = true;
                branch = true;
                explicit_long = false;
            }
            "--no-short" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
            }
            "--no-branch" => branch = false,
            "-uno" | "--untracked-files=no" | "--untracked-files=" => {
                untracked_mode = sley_worktree::StatusUntrackedMode::None;
            }
            "-unormal" | "--no-untracked-files" | "--untracked-files=normal" => {
                untracked_mode = sley_worktree::StatusUntrackedMode::Normal;
            }
            "-u" | "-uall" | "--untracked-files" | "--untracked-files=all" => {
                untracked_mode = sley_worktree::StatusUntrackedMode::All;
            }
            value if value.starts_with("-u") && value.len() > 2 => {
                return status_invalid_untracked_files_mode_error(&value[2..]);
            }
            value if value.starts_with("--untracked-files=") => {
                return status_invalid_untracked_files_mode_error(
                    &value["--untracked-files=".len()..],
                );
            }
            value if value.starts_with("--porcelain=") => {
                return status_unsupported_porcelain_version_error(&value["--porcelain=".len()..]);
            }
            "-z" | "--null" => {
                short = true;
                z = true;
            }
            "--no-null" => z = false,
            "--ignored" | "--ignored=traditional" | "--ignored=matching" => {
                show_ignored = true;
            }
            "--ignored=no" | "--no-ignored" => show_ignored = false,
            value if value.starts_with("--ignored=") => {
                return status_invalid_ignored_mode_error(&value["--ignored=".len()..]);
            }
            "--long" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = true;
            }
            "--no-long" => {
                short = false;
                porcelain_v1 = false;
                porcelain_v2 = false;
                explicit_long = false;
            }
            "--no-renames"
            | "--renames"
            | "--find-renames"
            | "-v"
            | "--verbose"
            | "--no-verbose"
            | "--column"
            | "--no-column"
            | "--column="
            | "--column=auto"
            | "--column=always"
            | "--column=never"
            | "--column=plain"
            | "--column=column"
            | "--column=row"
            | "--column=dense"
            | "--column=nodense"
            | "--ignore-submodules"
            | "--ignore-submodules=none"
            | "--ignore-submodules=untracked"
            | "--ignore-submodules=dirty"
            | "--ignore-submodules=all"
            | "--no-ignore-submodules" => {}
            "--ahead-behind" => ahead_behind = true,
            "--no-ahead-behind" => ahead_behind = false,
            "--show-stash" => show_stash = true,
            "--no-show-stash" => show_stash = false,
            "-M" => {}
            value if value.starts_with("-M") && value.len() > 2 => {}
            value if value.starts_with("--find-renames=") => {}
            value if value.starts_with("--short=") => {
                return status_option_takes_no_value_error("short");
            }
            value if value.starts_with("--no-short=") => {
                return status_option_takes_no_value_error("no-short");
            }
            value if value.starts_with("--no-porcelain=") => {
                return status_option_takes_no_value_error("no-porcelain");
            }
            value if value.starts_with("--branch=") => {
                return status_option_takes_no_value_error("branch");
            }
            value if value.starts_with("--no-branch=") => {
                return status_option_takes_no_value_error("no-branch");
            }
            value if value.starts_with("--null=") => {
                return status_option_takes_no_value_error("null");
            }
            value if value.starts_with("--no-null=") => {
                return status_option_takes_no_value_error("no-null");
            }
            value if value.starts_with("--no-ignored=") => {
                return status_option_takes_no_value_error("no-ignored");
            }
            value if value.starts_with("--long=") => {
                return status_option_takes_no_value_error("long");
            }
            value if value.starts_with("--no-long=") => {
                return status_option_takes_no_value_error("no-long");
            }
            value if value.starts_with("--ahead-behind=") => {
                return status_option_takes_no_value_error("ahead-behind");
            }
            value if value.starts_with("--no-ahead-behind=") => {
                return status_option_takes_no_value_error("no-ahead-behind");
            }
            value if value.starts_with("--verbose=") => {
                return status_option_takes_no_value_error("verbose");
            }
            value if value.starts_with("--no-verbose=") => {
                return status_option_takes_no_value_error("no-verbose");
            }
            value if value.starts_with("--show-stash=") => {
                return status_option_takes_no_value_error("show-stash");
            }
            value if value.starts_with("--no-show-stash=") => {
                return status_option_takes_no_value_error("no-show-stash");
            }
            value if value.starts_with("--renames=") => {
                return status_option_takes_no_value_error("no-no-renames");
            }
            value if value.starts_with("--no-renames=") => {
                return status_option_takes_no_value_error("no-renames");
            }
            value if value.starts_with("--column=") => {
                return status_unsupported_column_option_error(&value["--column=".len()..]);
            }
            value if value.starts_with("--no-column=") => {
                return status_option_takes_no_value_error("no-column");
            }
            value if value.starts_with("--ignore-submodules=") => {
                return status_bad_ignore_submodules_argument_error(
                    &value["--ignore-submodules=".len()..],
                );
            }
            value if value.starts_with("--no-ignore-submodules=") => {
                return status_option_takes_no_value_error("no-ignore-submodules");
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(
                    "status currently supports only --short, --porcelain, --porcelain=1, --porcelain=v1, --porcelain=v2, --long, --branch, -z/--null, --untracked-files, --ignored=no, --no-renames, simple display toggles, and literal pathspecs"
                        .into(),
                ));
            }
            _ => path_args.push(arg.clone()),
        }
    }
    if explicit_long && z {
        eprintln!("fatal: options '--long' and '-z' cannot be used together");
        return Err(GitError::Exit(128));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    // status needs a work tree; emit git's diagnostic (bare / no-worktree, or
    // the core.bare+core.worktree conflict) when one isn't available.
    let worktree_root = require_work_tree(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let mut entries = sley_worktree::short_status_with_options(
        &worktree_root,
        &git_dir,
        format,
        sley_worktree::ShortStatusOptions {
            include_ignored: show_ignored,
            untracked_mode,
        },
    )?;
    let pathspec = StatusPathspec::new(&cwd, &worktree_root, &path_args)?;
    if pathspec.has_filters() {
        entries.retain(|entry| pathspec.matches(&entry.path));
    }
    if !z && !porcelain_v1 {
        for entry in &mut entries {
            entry.path = pathspec.display(&entry.path);
        }
    }
    if porcelain_v2 {
        print_status_porcelain_v2(&git_dir, format, entries, branch, ahead_behind, z)?;
    } else if z {
        let mut stdout = io::stdout().lock();
        if branch {
            stdout.write_all(status_branch_header(&git_dir, format, ahead_behind)?.as_bytes())?;
            stdout.write_all(&[0])?;
        }
        for entry in entries {
            write!(stdout, "{}{} ", entry.index as char, entry.worktree as char)?;
            stdout.write_all(&entry.path)?;
            stdout.write_all(&[0])?;
        }
    } else if short {
        if branch {
            println!("{}", status_branch_header(&git_dir, format, ahead_behind)?);
        }
        for entry in entries {
            println!(
                "{}{} {}",
                entry.index as char,
                entry.worktree as char,
                status_quote_path(&entry.path, true)
            );
        }
    } else {
        print_status_long(&git_dir, format, entries, false, show_stash, ahead_behind)?;
    }
    Ok(())
}

fn status_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn status_invalid_untracked_files_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid untracked files mode '{mode}'");
    Err(GitError::Exit(128))
}

fn status_invalid_ignored_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid ignored mode '{mode}'");
    Err(GitError::Exit(128))
}

fn status_unsupported_porcelain_version_error(version: &str) -> Result<()> {
    eprintln!("fatal: unsupported porcelain version '{version}'");
    Err(GitError::Exit(128))
}

fn status_bad_ignore_submodules_argument_error(value: &str) -> Result<()> {
    eprintln!("fatal: bad --ignore-submodules argument: {value}");
    Err(GitError::Exit(128))
}

fn status_unsupported_column_option_error(value: &str) -> Result<()> {
    eprintln!("error: unsupported option '{value}'");
    Err(GitError::Exit(129))
}

struct StatusPathspec {
    prefix: Vec<u8>,
    filters: Vec<LsFilesPathFilter>,
    cwd_depth: usize,
}

impl StatusPathspec {
    fn new(cwd: &Path, worktree_root: &Path, path_args: &[String]) -> Result<Self> {
        let root = fs::canonicalize(worktree_root)?;
        let cwd = fs::canonicalize(cwd)?;
        let relative = cwd.strip_prefix(&root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", cwd.display()))
        })?;
        let prefix = relative.to_string_lossy().replace('\\', "/").into_bytes();
        let cwd_depth = path_component_count(&prefix);
        let mut filters = Vec::new();
        for arg in path_args {
            let filter_path = normalize_ls_files_pathspec(&prefix, arg)?;
            let is_glob = sley_worktree::pathspec_is_glob(&filter_path);
            let arg_path = Path::new(arg);
            let absolute = if arg_path.is_absolute() {
                arg_path.to_path_buf()
            } else {
                cwd.join(arg_path)
            };
            filters.push(LsFilesPathFilter {
                original: arg.clone(),
                path: filter_path,
                recursive: arg == "." || arg.ends_with('/') || absolute.is_dir(),
                is_glob,
                matched: Cell::new(false),
            });
        }
        Ok(Self {
            prefix,
            filters,
            cwd_depth,
        })
    }

    fn has_filters(&self) -> bool {
        !self.filters.is_empty()
    }

    fn display(&self, path: &[u8]) -> Vec<u8> {
        if self.prefix.is_empty() {
            return path.to_vec();
        }
        if let Some(rest) = path.strip_prefix(self.prefix.as_slice())
            && let Some(rest) = rest.strip_prefix(b"/")
        {
            return rest.to_vec();
        }
        let mut display = Vec::new();
        for _ in 0..self.cwd_depth {
            display.extend_from_slice(b"../");
        }
        display.extend_from_slice(path);
        display
    }

    fn matches(&self, path: &[u8]) -> bool {
        let magic = effective_pathspec_flags();
        self.filters.iter().any(|filter| filter.matches(path, magic))
    }
}

fn print_status_porcelain_v2(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    branch: bool,
    ahead_behind: bool,
    z: bool,
) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let separator = if z { b'\0' } else { b'\n' };
    if branch {
        for header in status_porcelain_v2_branch_headers(git_dir, format, ahead_behind)? {
            stdout.write_all(header.as_bytes())?;
            stdout.write_all(&[separator])?;
        }
    }
    let zero = zero_oid(format)?;
    for entry in entries {
        if entry.index == b'!' && entry.worktree == b'!' {
            stdout.write_all(b"! ")?;
            if z {
                stdout.write_all(&entry.path)?;
            } else {
                stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
            }
            stdout.write_all(&[separator])?;
            continue;
        }
        if entry.index == b'?' && entry.worktree == b'?' {
            stdout.write_all(b"? ")?;
            if z {
                stdout.write_all(&entry.path)?;
            } else {
                stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
            }
            stdout.write_all(&[separator])?;
            continue;
        }
        let index = status_porcelain_v2_code(entry.index);
        let worktree = status_porcelain_v2_code(entry.worktree);
        write!(
            stdout,
            "1 {index}{worktree} N... {:06o} {:06o} {:06o} {} {} ",
            entry.head_mode.unwrap_or(0),
            entry.index_mode.unwrap_or(0),
            entry.worktree_mode.unwrap_or(0),
            entry.head_oid.as_ref().unwrap_or(&zero).to_hex(),
            entry.index_oid.as_ref().unwrap_or(&zero).to_hex()
        )?;
        if z {
            stdout.write_all(&entry.path)?;
        } else {
            stdout.write_all(status_quote_path(&entry.path, false).as_bytes())?;
        }
        stdout.write_all(&[separator])?;
    }
    stdout.flush()?;
    Ok(())
}

fn print_status_long(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<sley_worktree::ShortStatusEntry>,
    commit_preview: bool,
    show_stash: bool,
    ahead_behind: bool,
) -> Result<()> {
    let head_initial = print_status_long_branch(git_dir, format, ahead_behind)?;
    if head_initial {
        println!();
        if commit_preview {
            println!("Initial commit");
        } else {
            println!("No commits yet");
        }
    }

    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    for entry in entries {
        if entry.index == b'?' && entry.worktree == b'?' {
            untracked.push(entry.path);
            continue;
        }
        if entry.index == b'!' && entry.worktree == b'!' {
            ignored.push(entry.path);
            continue;
        }
        if let Some(label) = status_long_change_label(entry.index) {
            staged.push((label, entry.path.clone()));
        }
        if let Some(label) = status_long_change_label(entry.worktree) {
            unstaged.push((label, entry.path));
        }
    }

    let has_staged = !staged.is_empty();
    let has_unstaged = !unstaged.is_empty();
    let has_untracked = !untracked.is_empty();
    let has_ignored = !ignored.is_empty();

    if has_staged {
        if head_initial {
            println!();
        }
        println!("Changes to be committed:");
        if head_initial {
            println!("  (use \"git rm --cached <file>...\" to unstage)");
        } else {
            println!("  (use \"git restore --staged <file>...\" to unstage)");
        }
        for (label, path) in staged {
            println!("\t{label:<12}{}", status_quote_path(&path, false));
        }
    }

    if has_unstaged {
        if head_initial || has_staged {
            println!();
        }
        println!("Changes not staged for commit:");
        if unstaged.iter().any(|(label, _)| *label == "deleted:") {
            println!("  (use \"git add/rm <file>...\" to update what will be committed)");
        } else {
            println!("  (use \"git add <file>...\" to update what will be committed)");
        }
        println!("  (use \"git restore <file>...\" to discard changes in working directory)");
        for (label, path) in unstaged {
            println!("\t{label:<12}{}", status_quote_path(&path, false));
        }
    }

    if has_untracked {
        if head_initial || has_staged || has_unstaged {
            println!();
        }
        println!("Untracked files:");
        println!("  (use \"git add <file>...\" to include in what will be committed)");
        for path in untracked {
            println!("\t{}", status_quote_path(&path, false));
        }
    }

    if has_ignored {
        if head_initial || has_staged || has_unstaged || has_untracked {
            println!();
        }
        println!("Ignored files:");
        println!("  (use \"git add -f <file>...\" to include in what will be committed)");
        for path in ignored {
            println!("\t{}", status_quote_path(&path, false));
        }
    }

    if !has_staged && !has_unstaged && !has_untracked && !has_ignored {
        if head_initial {
            println!();
            println!("nothing to commit (create/copy files and use \"git add\" to track)");
        } else {
            println!("nothing to commit, working tree clean");
        }
    } else if !has_staged && has_unstaged {
        println!();
        println!("no changes added to commit (use \"git add\" and/or \"git commit -a\")");
    } else if !has_staged && has_untracked {
        println!();
        println!("nothing added to commit but untracked files present (use \"git add\" to track)");
    } else {
        println!();
    }
    if show_stash {
        let stash_count = status_stash_count(git_dir, format)?;
        if stash_count == 1 {
            println!("Your stash currently has 1 entry");
        } else if stash_count > 1 {
            println!("Your stash currently has {stash_count} entries");
        }
    }
    Ok(())
}

fn status_stash_count(git_dir: &Path, format: ObjectFormat) -> Result<usize> {
    let store = FileRefStore::new(git_dir, format);
    Ok(store.read_reflog("refs/stash")?.len())
}

fn print_status_long_branch(
    git_dir: &Path,
    format: ObjectFormat,
    ahead_behind: bool,
) -> Result<bool> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            if let Some(branch) = target.strip_prefix("refs/heads/") {
                println!("On branch {branch}");
                if let Some(RefTarget::Direct(oid)) = store.read_ref(&target)? {
                    print_status_long_tracking(
                        git_dir,
                        format,
                        &store,
                        &target,
                        &oid,
                        ahead_behind,
                    )?;
                    Ok(false)
                } else {
                    Ok(true)
                }
            } else {
                println!("On branch {target}");
                Ok(store.read_ref(&target)?.is_none())
            }
        }
        Some(RefTarget::Direct(oid)) => {
            println!("HEAD detached at {}", format_log_abbrev_oid(&oid));
            Ok(false)
        }
        None => {
            println!("On branch (unknown)");
            Ok(true)
        }
    }
}

fn print_status_long_tracking(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch_ref: &str,
    oid: &ObjectId,
    ahead_behind: bool,
) -> Result<()> {
    let Some(tracking) =
        status_branch_tracking(git_dir, format, store, branch_ref, oid, ahead_behind)?
    else {
        return Ok(());
    };
    match tracking.state {
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0,
            behind: 0,
            ..
        }) => {
            println!("Your branch is up to date with '{}'.", tracking.upstream);
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead, behind: 0, ..
        }) => {
            println!(
                "Your branch is ahead of '{}' by {ahead} {}.",
                tracking.upstream,
                status_commit_word(ahead)
            );
            println!("  (use \"git push\" to publish your local commits)");
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0, behind, ..
        }) => {
            println!(
                "Your branch is behind '{}' by {behind} {}, and can be fast-forwarded.",
                tracking.upstream,
                status_commit_word(behind)
            );
            println!("  (use \"git pull\" to update your local branch)");
        }
        StatusBranchTrackingState::Counts(ForEachRefTrack { ahead, behind, .. }) => {
            println!("Your branch and '{}' have diverged,", tracking.upstream);
            println!("and have {ahead} and {behind} different commits each, respectively.");
            println!("  (use \"git pull\" if you want to integrate the remote branch with yours)");
        }
        StatusBranchTrackingState::Different => {
            println!(
                "Your branch and '{}' refer to different commits.",
                tracking.upstream
            );
            println!("  (use \"git status --ahead-behind\" for details)");
        }
        StatusBranchTrackingState::Gone => {
            println!(
                "Your branch is based on '{}', but the upstream is gone.",
                tracking.upstream
            );
            println!("  (use \"git branch --unset-upstream\" to fixup)");
        }
    }
    println!();
    Ok(())
}

fn status_commit_word(count: usize) -> &'static str {
    if count == 1 { "commit" } else { "commits" }
}

fn status_long_change_label(code: u8) -> Option<&'static str> {
    match code {
        b'A' => Some("new file:"),
        b'M' => Some("modified:"),
        b'D' => Some("deleted:"),
        _ => None,
    }
}

fn status_entries_have_index_changes(entries: &[sley_worktree::ShortStatusEntry]) -> bool {
    entries
        .iter()
        .any(|entry| status_long_change_label(entry.index).is_some())
}

fn status_porcelain_v2_code(code: u8) -> char {
    if code == b' ' { '.' } else { code as char }
}

fn status_porcelain_v2_branch_headers(
    git_dir: &Path,
    format: ObjectFormat,
    ahead_behind: bool,
) -> Result<Vec<String>> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            let target_oid = match store.read_ref(&target)? {
                Some(RefTarget::Direct(oid)) => Some(oid),
                _ => None,
            };
            let oid = match target_oid.as_ref() {
                Some(oid) => oid.to_hex(),
                _ => "(initial)".into(),
            };
            let head = target
                .strip_prefix("refs/heads/")
                .unwrap_or(target.as_str())
                .to_string();
            let mut headers = vec![
                format!("# branch.oid {oid}"),
                format!("# branch.head {head}"),
            ];
            if let Some(oid) = target_oid.as_ref()
                && let Some(tracking) =
                    status_branch_tracking(git_dir, format, &store, &target, oid, ahead_behind)?
            {
                headers.push(format!("# branch.upstream {}", tracking.upstream));
                match tracking.state {
                    StatusBranchTrackingState::Counts(track) => {
                        headers.push(format!("# branch.ab +{} -{}", track.ahead, track.behind));
                    }
                    StatusBranchTrackingState::Different => {
                        headers.push("# branch.ab +? -?".into());
                    }
                    StatusBranchTrackingState::Gone => {}
                }
            }
            Ok(headers)
        }
        Some(RefTarget::Direct(oid)) => Ok(vec![
            format!("# branch.oid {}", oid.to_hex()),
            "# branch.head (detached)".into(),
        ]),
        None => Ok(vec![
            "# branch.oid (initial)".into(),
            "# branch.head (unknown)".into(),
        ]),
    }
}

fn status_branch_header(
    git_dir: &Path,
    format: ObjectFormat,
    ahead_behind: bool,
) -> Result<String> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            if let Some(branch) = target.strip_prefix("refs/heads/") {
                if let Some(RefTarget::Direct(oid)) = store.read_ref(&target)? {
                    let mut header = format!("## {branch}");
                    if let Some(tracking) = status_branch_tracking(
                        git_dir,
                        format,
                        &store,
                        &target,
                        &oid,
                        ahead_behind,
                    )? {
                        header.push_str("...");
                        header.push_str(&tracking.upstream);
                        if let StatusBranchTrackingState::Counts(track) = tracking.state {
                            if track.ahead > 0 || track.behind > 0 {
                                header.push(' ');
                                let mut suffix = Vec::new();
                                write_for_each_ref_track(&mut suffix, track, true)?;
                                header.push_str(&String::from_utf8_lossy(&suffix));
                            }
                        } else if matches!(tracking.state, StatusBranchTrackingState::Gone) {
                            header.push_str(" [gone]");
                        } else {
                            header.push_str(" [different]");
                        }
                    }
                    Ok(header)
                } else {
                    Ok(format!("## No commits yet on {branch}"))
                }
            } else {
                Ok(format!("## {target}"))
            }
        }
        Some(RefTarget::Direct(_)) | None => Ok("## HEAD (no branch)".into()),
    }
}

struct StatusBranchTracking {
    upstream: String,
    state: StatusBranchTrackingState,
}

#[derive(Clone, Copy)]
enum StatusBranchTrackingState {
    Counts(ForEachRefTrack),
    Different,
    Gone,
}

fn status_branch_tracking(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch_ref: &str,
    oid: &ObjectId,
    ahead_behind: bool,
) -> Result<Option<StatusBranchTracking>> {
    let config = read_repo_config(git_dir)?;
    let Some(upstream) = for_each_ref_upstream(&config, branch_ref) else {
        return Ok(None);
    };
    let db = FileObjectDatabase::new(repository_objects_dir(git_dir), format);
    let track = if ahead_behind {
        match store.read_ref(&upstream.refname)? {
            None => StatusBranchTrackingState::Gone,
            Some(_) => for_each_ref_upstream_track(store, &db, format, oid, &upstream.refname)?
                .map(StatusBranchTrackingState::Counts)
                .unwrap_or(StatusBranchTrackingState::Different),
        }
    } else {
        status_branch_tracking_without_ahead_behind(store, oid, &upstream.refname)?
    };
    Ok(Some(StatusBranchTracking {
        upstream: for_each_ref_short_name(&upstream.refname).to_string(),
        state: track,
    }))
}

fn status_branch_tracking_without_ahead_behind(
    store: &FileRefStore,
    oid: &ObjectId,
    upstream: &str,
) -> Result<StatusBranchTrackingState> {
    let Some(RefTarget::Direct(upstream_oid)) = store.read_ref(upstream)? else {
        return Ok(StatusBranchTrackingState::Gone);
    };
    if oid == &upstream_oid {
        Ok(StatusBranchTrackingState::Counts(ForEachRefTrack {
            ahead: 0,
            behind: 0,
            gone: false,
        }))
    } else {
        Ok(StatusBranchTrackingState::Different)
    }
}

fn build_commit_author_identity(author: Option<&str>, date: Option<&str>) -> Result<Vec<u8>> {
    let (name, email) = if let Some(author) = author {
        parse_commit_author(author)?
    } else {
        // Same precedence as `commit_identity_from_env`: env var, then
        // `-c`/`GIT_CONFIG_*`, then effective config (repo > global > system),
        // then the built-in default.
        let env_name = env::var("GIT_AUTHOR_NAME").ok();
        let env_email = env::var("GIT_AUTHOR_EMAIL").ok();
        let mut config = if env_name.is_none() || env_email.is_none() {
            IdentityConfig::Lazy(None)
        } else {
            IdentityConfig::Skip
        };
        let name = env_name
            .or_else(|| identity_config_value("user.name", &mut config))
            .unwrap_or_else(|| "Git Rs".into());
        let email = env_email
            .or_else(|| identity_config_value("user.email", &mut config))
            .unwrap_or_else(|| "sley@example.invalid".into());
        (name, email)
    };
    let date = date
        .map(str::to_string)
        .unwrap_or_else(|| env::var("GIT_AUTHOR_DATE").unwrap_or_else(|_| "@0 +0000".into()));
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity(&name, &email, &date)
}

fn parse_commit_author(author: &str) -> Result<(String, String)> {
    let Some((name, rest)) = author.rsplit_once('<') else {
        return commit_invalid_author_error(author);
    };
    let Some(email) = rest.strip_suffix('>') else {
        return commit_invalid_author_error(author);
    };
    let name = name.trim_end();
    if name.is_empty() || email.is_empty() {
        return commit_invalid_author_error(author);
    }
    Ok((name.to_string(), email.to_string()))
}

fn commit_invalid_author_error(author: &str) -> Result<(String, String)> {
    eprintln!("fatal: --author '{author}' is not 'Name <email>' and matches no existing author");
    Err(GitError::Exit(128))
}
