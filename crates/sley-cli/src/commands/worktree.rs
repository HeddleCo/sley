//! `git worktree`: manage multiple working trees attached to one repository.

use crate::*;

pub(crate) fn cmd_worktree(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        eprintln!("error: need a subcommand");
        return worktree_usage();
    };
    match subcommand {
        "add" => cmd_worktree_add(&args[1..]),
        "list" => cmd_worktree_list(&args[1..]),
        "prune" => cmd_worktree_prune(&args[1..]),
        "lock" => cmd_worktree_lock(&args[1..]),
        "move" => cmd_worktree_move(&args[1..]),
        "remove" => cmd_worktree_remove(&args[1..]),
        "repair" => cmd_worktree_repair(&args[1..]),
        "unlock" => cmd_worktree_unlock(&args[1..]),
        _ => {
            eprintln!("error: unknown subcommand: '{subcommand}'");
            worktree_usage()
        }
    }
}

#[derive(Debug)]
struct WorktreeListOptions {
    porcelain: bool,
    verbose: bool,
    z: bool,
    expire: bool,
}

#[derive(Debug)]
struct WorktreeListEntry {
    path: String,
    head: ObjectId,
    branch: Option<String>,
    detached: bool,
    prunable_reason: Option<String>,
    locked_reason: Option<String>,
}

#[derive(Debug)]
struct LinkedWorktreeAdmin {
    admin_dir: PathBuf,
    admin_name: String,
    path: PathBuf,
    prunable_reason: Option<String>,
    locked_reason: Option<String>,
}

#[derive(Debug)]
struct WorktreePruneOptions {
    dry_run: bool,
    verbose: bool,
    expire: bool,
}

#[derive(Debug)]
struct WorktreeLockOptions {
    reason: Option<String>,
    path: String,
}

#[derive(Debug)]
struct WorktreeRemoveOptions {
    force: usize,
    path: String,
}

#[derive(Debug)]
struct WorktreeMoveOptions {
    force: usize,
    source: String,
    destination: String,
}

#[derive(Debug)]
struct WorktreeAddOptions {
    force: usize,
    quiet: bool,
    detach: bool,
    checkout: bool,
    lock: bool,
    lock_reason: Option<String>,
    /// The branch name to create/reset (`-b` or `-B`). When `--orphan` is set
    /// without an explicit `-b`/`-B`, this is filled in later with the
    /// worktree basename.
    branch: Option<String>,
    /// `-B` was used (create-or-reset). Distinct from `-b` (create-only).
    force_branch: bool,
    /// Explicit `--orphan` flag (NOT the DWIM-inferred orphan, which is tracked
    /// separately on the resolved head).
    orphan: bool,
    guess_remote: bool,
    /// `--track`/`--no-track` as a tri-state mirror of git's `OPT_PASSTHRU`
    /// `opt_track`: `None` when neither flag given, `Some(true)` for `--track`,
    /// `Some(false)` for `--no-track`. Git only treats a *present* `opt_track`
    /// (either form) as "tracking requested" for the orphan-conflict check.
    track: Option<bool>,
    /// Tri-state `--relative-paths`/`--no-relative-paths`. `None` falls back to
    /// the `worktree.useRelativePaths` config default at use time.
    relative_paths: Option<bool>,
    path: String,
    start: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeTrackedEntry {
    mode: u32,
    oid: ObjectId,
}

pub(crate) fn cmd_worktree_add(args: &[String]) -> Result<()> {
    let mut options = parse_worktree_add_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let path = resolve_cli_path(&cwd, &options.path);
    validate_worktree_add_destination(&path, &options.path)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let committer = commit_identity_from_env("COMMITTER")?;

    // `--orphan` with no explicit `-b`/`-B` names the new unborn branch after
    // the worktree basename (git: `opts.orphan && !new_branch`).
    if options.orphan && options.branch.is_none() {
        options.branch = Some(default_worktree_add_branch_name(&path)?);
    }

    // `-B`: if the branch already exists and is checked out elsewhere, die
    // before doing any work (git's `new_branch_force` arm calls
    // `die_if_checked_out` up front).
    if options.force_branch
        && let Some(branch) = options.branch.as_ref()
    {
        let refname = branch_ref_name(branch)?;
        if store.read_ref(&refname)?.is_some()
            && let Some(existing_path) =
                branch_checked_out_worktree(&common_git_dir, &refname, Some(&path))?
            && options.force == 0
        {
            eprintln!(
                "fatal: '{}' is already used by worktree at '{}'",
                branch,
                existing_path.display()
            );
            return Err(GitError::Exit(128));
        }
    }
    let add_head = worktree_add_resolve_head(
        &common_git_dir,
        format,
        &store,
        &path,
        &options,
        committer.clone(),
    )?;
    if let Some(branch) = add_head.branch_name.as_ref() {
        let refname = branch_ref_name(branch)?;
        if let Some(existing_path) =
            branch_checked_out_worktree(&common_git_dir, &refname, Some(&path))?
            && options.force == 0
        {
            if !options.quiet {
                eprintln!("{}", add_head.prepare_message);
            }
            eprintln!(
                "fatal: '{}' is already used by worktree at '{}'",
                branch,
                existing_path.display()
            );
            return Err(GitError::Exit(128));
        }
    }
    // git prints the "Preparing worktree ..." line in `add()` and then runs
    // `check_candidate_path` as the first step of `add_worktree`, so the
    // missing-but-registered / already-exists fatals appear *after* the prepare
    // line. Emit the prepare line here (for the non-orphan path) to match, then
    // run the candidate-path check before allocating the admin dir.
    if !options.quiet && !add_head.orphan {
        eprintln!("{}", add_head.prepare_message);
    }
    check_worktree_candidate_path(&common_git_dir, &path, &options.path, options.force)?;
    // `--relative-paths`/`--no-relative-paths` override the `worktree.useRelativePaths`
    // config default (git: `opts.relative_paths = use_relative_paths`).
    let relative_paths = options.relative_paths.unwrap_or_else(|| {
        GitConfig::read(common_git_dir.join("config"))
            .ok()
            .and_then(|config| config.get_bool("worktree", None, "useRelativePaths"))
            .unwrap_or(false)
    });
    let admin_dir = create_linked_worktree_admin_dir(&common_git_dir, &path)?;
    fs::create_dir_all(&path)?;
    write_worktree_linking_files(&common_git_dir, &admin_dir, &path, relative_paths)?;
    fs::write(admin_dir.join("commondir"), "../..\n")?;
    // An inferred-orphan worktree has no source commit, so — matching git — it
    // writes no ORIG_HEAD and points HEAD at the unborn branch.
    if !add_head.orphan {
        fs::write(admin_dir.join("ORIG_HEAD"), format!("{}\n", add_head.oid))?;
    }
    match add_head.branch_name.as_ref() {
        Some(branch) => fs::write(
            admin_dir.join("HEAD"),
            format!("ref: {}\n", branch_ref_name(branch)?),
        )?,
        None => fs::write(admin_dir.join("HEAD"), format!("{}\n", add_head.oid))?,
    }
    if options.lock {
        fs::write(
            admin_dir.join("locked"),
            options
                .lock_reason
                .as_deref()
                .map(|reason| format!("{reason}\n"))
                .unwrap_or_default(),
        )?;
    }
    if add_head.orphan {
        // Orphan worktrees check out the empty tree: write an empty index (with
        // the empty-tree cache-tree, byte-for-byte like git) and no files. No
        // "HEAD is now at" reset line is printed since there is no commit.
        write_empty_worktree_index(&admin_dir, format)?;
        if !options.quiet {
            eprintln!("{}", add_head.prepare_message);
        }
        return Ok(());
    }
    write_linked_worktree_checkout(
        &common_git_dir,
        &admin_dir,
        &path,
        format,
        &add_head.oid,
        options.checkout,
    )?;
    // The prepare line was already printed (before check_candidate_path); only
    // the post-checkout "HEAD is now at ..." reset line remains.
    if !options.quiet && options.checkout {
        print_reset_hard_head(&common_git_dir, format, &add_head.oid)?;
    }
    Ok(())
}

pub(crate) fn cmd_worktree_list(args: &[String]) -> Result<()> {
    let options = parse_worktree_list_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let entries = collect_worktree_list_entries(&common_git_dir, format, options.expire)?;
    if options.porcelain {
        print_worktree_list_porcelain(&entries, options.z)?;
    } else {
        print_worktree_list_default(&entries, options.verbose);
    }
    Ok(())
}

fn parse_worktree_list_options(args: &[String]) -> Result<WorktreeListOptions> {
    let mut porcelain = false;
    let mut verbose = false;
    let mut z = false;
    let mut expire = true;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--porcelain" => porcelain = true,
            "--no-porcelain" => porcelain = false,
            "-z" => z = true,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "--expire" => {
                index += 1;
                if args.get(index).is_none() {
                    eprintln!("error: option `expire' requires a value");
                    return Err(GitError::Exit(129));
                }
                expire = true;
            }
            value if value.starts_with("--expire=") => expire = true,
            "--no-expire" => expire = false,
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return worktree_list_usage();
            }
            _ => return worktree_list_usage(),
        }
        index += 1;
    }
    if z && !porcelain {
        eprintln!("fatal: the option '-z' requires '--porcelain'");
        return Err(GitError::Exit(128));
    }
    Ok(WorktreeListOptions {
        porcelain,
        verbose,
        z,
        expire,
    })
}

fn worktree_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git worktree add [-f] [--detach] [--checkout] [--lock [--reason <string>]]\n                        [--orphan] [(-b | -B) <new-branch>] <path> [<commit-ish>]\n   or: git worktree list [-v | --porcelain [-z]]\n   or: git worktree lock [--reason <string>] <worktree>\n   or: git worktree move <worktree> <new-path>\n   or: git worktree prune [-n] [-v] [--expire <expire>]\n   or: git worktree remove [-f] <worktree>\n   or: git worktree repair [<path>...]\n   or: git worktree unlock <worktree>"
    );
    Err(GitError::Exit(129))
}

fn worktree_list_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree list [-v | --porcelain [-z]]");
    eprintln!();
    eprintln!("    --[no-]porcelain      machine-readable output");
    eprintln!("    -v, --[no-]verbose    show extended annotations and reasons, if available");
    eprintln!(
        "    --[no-]expire <expiry-date>\n                          add 'prunable' annotation to missing worktrees older than <time>"
    );
    eprintln!("    -z                    terminate records with a NUL character");
    Err(GitError::Exit(129))
}

pub(crate) fn cmd_worktree_prune(args: &[String]) -> Result<()> {
    let options = parse_worktree_prune_options(args)?;
    if !options.expire {
        return Ok(());
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    for admin in collect_linked_worktree_admins(&common_git_dir)? {
        if admin.locked_reason.is_some() {
            continue;
        }
        let Some(reason) = admin.prunable_reason else {
            continue;
        };
        if options.dry_run || options.verbose {
            eprintln!("Removing worktrees/{}: {}", admin.admin_name, reason);
        }
        if !options.dry_run {
            fs::remove_dir_all(&admin.admin_dir)?;
        }
    }
    Ok(())
}

pub(crate) fn cmd_worktree_lock(args: &[String]) -> Result<()> {
    let options = parse_worktree_lock_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let admin = find_linked_worktree_admin(&common_git_dir, &cwd, &options.path)?;
    if admin.locked_reason.is_some() {
        eprint!("fatal: '{}' is already locked", options.path);
        if let Some(reason) = admin.locked_reason.filter(|reason| !reason.is_empty()) {
            eprint!(", reason: {reason}");
        }
        eprintln!();
        return Err(GitError::Exit(128));
    }
    let contents = options
        .reason
        .map(|reason| format!("{reason}\n"))
        .unwrap_or_default();
    fs::write(admin.admin_dir.join("locked"), contents)?;
    Ok(())
}

pub(crate) fn cmd_worktree_unlock(args: &[String]) -> Result<()> {
    let path = parse_worktree_unlock_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let admin = find_linked_worktree_admin(&common_git_dir, &cwd, &path)?;
    if admin.locked_reason.is_none() {
        eprintln!("fatal: '{path}' is not locked");
        return Err(GitError::Exit(128));
    }
    fs::remove_file(admin.admin_dir.join("locked"))?;
    Ok(())
}

pub(crate) fn cmd_worktree_remove(args: &[String]) -> Result<()> {
    let options = parse_worktree_remove_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let admin = find_linked_worktree_admin_for_remove(&common_git_dir, &cwd, &options.path)?;
    if let Some(reason) = admin.locked_reason.as_ref()
        && options.force < 2
    {
        eprint!("fatal: cannot remove a locked working tree");
        if !reason.is_empty() {
            eprint!(", lock reason: {reason}");
        }
        eprintln!();
        eprintln!("use 'remove -f -f' to override or unlock first");
        return Err(GitError::Exit(128));
    }
    if options.force == 0 && worktree_remove_has_local_changes(&common_git_dir, &admin, format)? {
        eprintln!(
            "fatal: '{}' contains modified or untracked files, use --force to delete it",
            options.path
        );
        return Err(GitError::Exit(128));
    }
    if admin.path.exists() {
        fs::remove_dir_all(&admin.path)?;
    }
    if admin.admin_dir.exists() {
        fs::remove_dir_all(&admin.admin_dir)?;
    }
    Ok(())
}

pub(crate) fn cmd_worktree_move(args: &[String]) -> Result<()> {
    let options = parse_worktree_move_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let admin = find_linked_worktree_admin_for_move(&common_git_dir, &cwd, &options.source)?;
    if let Some(reason) = admin.locked_reason.as_ref()
        && options.force < 2
    {
        eprint!("fatal: cannot move a locked working tree");
        if !reason.is_empty() {
            eprint!(", lock reason: {reason}");
        }
        eprintln!();
        eprintln!("use 'move -f -f' to override or unlock first");
        return Err(GitError::Exit(128));
    }
    let destination = worktree_move_destination(&cwd, &admin.path, &options.destination)?;
    fs::rename(&admin.path, &destination)?;
    let dot_git = destination.join(".git");
    fs::write(
        admin.admin_dir.join("gitdir"),
        format!("{}\n", dot_git.display()),
    )?;
    Ok(())
}

pub(crate) fn cmd_worktree_repair(args: &[String]) -> Result<()> {
    let paths = parse_worktree_repair_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    if paths.is_empty() {
        repair_worktree_path(&common_git_dir, &cwd, None)?;
    } else {
        for path in paths {
            repair_worktree_path(&common_git_dir, &resolve_cli_path(&cwd, &path), Some(&path))?;
        }
    }
    Ok(())
}

fn parse_worktree_prune_options(args: &[String]) -> Result<WorktreePruneOptions> {
    let mut dry_run = false;
    let mut verbose = false;
    let mut expire = true;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "--expire" => {
                index += 1;
                if args.get(index).is_none() {
                    eprintln!("error: option `expire' requires a value");
                    return Err(GitError::Exit(129));
                }
                expire = true;
            }
            value if value.starts_with("--expire=") => expire = true,
            "--no-expire" => expire = false,
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return worktree_prune_usage();
            }
            _ => return worktree_prune_usage(),
        }
        index += 1;
    }
    Ok(WorktreePruneOptions {
        dry_run,
        verbose,
        expire,
    })
}

fn worktree_prune_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree prune [-n] [-v] [--expire <expire>]");
    eprintln!();
    eprintln!("    -n, --[no-]dry-run    do not remove, show only");
    eprintln!("    -v, --[no-]verbose    report pruned working trees");
    eprintln!(
        "    --[no-]expire <expiry-date>\n                          prune missing working trees older than <time>"
    );
    Err(GitError::Exit(129))
}

fn parse_worktree_lock_options(args: &[String]) -> Result<WorktreeLockOptions> {
    let mut reason = None;
    let mut paths = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--reason" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("error: option `reason' requires a value");
                    return Err(GitError::Exit(129));
                };
                reason = Some(value.clone());
            }
            value if let Some(value) = value.strip_prefix("--reason=") => {
                reason = Some(value.to_string());
            }
            "--no-reason" => reason = Some("(null)".to_string()),
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return worktree_lock_usage();
            }
            value => paths.push(value.to_string()),
        }
        index += 1;
    }
    if paths.len() != 1 {
        return worktree_lock_usage();
    }
    Ok(WorktreeLockOptions {
        reason,
        path: paths.remove(0),
    })
}

fn parse_worktree_remove_options(args: &[String]) -> Result<WorktreeRemoveOptions> {
    let mut force = 0usize;
    let mut paths = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => force += 1,
            "--no-force" => force = 0,
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return worktree_remove_usage();
            }
            value => paths.push(value.to_string()),
        }
    }
    if paths.len() != 1 {
        return worktree_remove_usage();
    }
    Ok(WorktreeRemoveOptions {
        force,
        path: paths.remove(0),
    })
}

fn parse_worktree_move_options(args: &[String]) -> Result<WorktreeMoveOptions> {
    let mut force = 0usize;
    let mut paths = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => force += 1,
            "--no-force" => force = 0,
            "--relative-paths" | "--no-relative-paths" => {}
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return worktree_move_usage();
            }
            value => paths.push(value.to_string()),
        }
    }
    if paths.len() != 2 {
        return worktree_move_usage();
    }
    Ok(WorktreeMoveOptions {
        force,
        source: paths.remove(0),
        destination: paths.remove(0),
    })
}

fn parse_worktree_repair_options(args: &[String]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--relative-paths" | "--no-relative-paths" => {}
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return worktree_repair_usage();
            }
            value => paths.push(value.to_string()),
        }
    }
    Ok(paths)
}

fn parse_worktree_unlock_options(args: &[String]) -> Result<String> {
    if args.len() != 1 || args[0].starts_with('-') {
        return worktree_unlock_usage();
    }
    Ok(args[0].clone())
}

fn worktree_lock_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree lock [--reason <string>] <worktree>");
    eprintln!();
    eprintln!("    --[no-]reason <string>");
    eprintln!("                          reason for locking");
    eprintln!();
    Err(GitError::Exit(129))
}

fn worktree_unlock_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree unlock <worktree>");
    eprintln!();
    Err(GitError::Exit(129))
}

fn worktree_remove_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree remove [-f] <worktree>");
    eprintln!();
    eprintln!("    -f, --[no-]force      force removal even if worktree is dirty or locked");
    eprintln!();
    Err(GitError::Exit(129))
}

fn worktree_move_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree move <worktree> <new-path>");
    eprintln!();
    eprintln!("    -f, --[no-]force      force move even if worktree is dirty or locked");
    eprintln!("    --[no-]relative-paths use relative paths for worktrees");
    eprintln!();
    Err(GitError::Exit(129))
}

fn worktree_repair_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree repair [<path>...]");
    eprintln!();
    eprintln!("    --[no-]relative-paths use relative paths for worktrees");
    eprintln!();
    Err(GitError::Exit(129))
}

fn parse_worktree_add_options(args: &[String]) -> Result<WorktreeAddOptions> {
    let mut force = 0usize;
    let mut quiet = false;
    let mut detach = false;
    let mut checkout = true;
    let mut keep_locked = false;
    let mut lock_reason: Option<String> = None;
    // `-b` and `-B` are tracked separately so their simultaneous use (and use
    // alongside `--detach`) is a "mutually exclusive options" error, matching
    // git's `!!opts.detach + !!new_branch + !!new_branch_force > 1` check.
    let mut new_branch: Option<String> = None;
    let mut new_branch_force: Option<String> = None;
    let mut orphan = false;
    let mut guess_remote = false;
    let mut track: Option<bool> = None;
    let mut relative_paths: Option<bool> = None;
    let mut paths = Vec::new();
    let mut saw_double_dash = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if saw_double_dash {
            paths.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => saw_double_dash = true,
            "-f" | "--force" => force += 1,
            "--no-force" => force = 0,
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-d" | "--detach" => detach = true,
            "--no-detach" => detach = false,
            "--checkout" => checkout = true,
            "--no-checkout" => checkout = false,
            "--orphan" => orphan = true,
            "--no-orphan" => orphan = false,
            "--lock" => keep_locked = true,
            "--no-lock" => keep_locked = false,
            "--reason" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("error: option `reason' requires a value");
                    return Err(GitError::Exit(129));
                };
                lock_reason = Some(value.clone());
            }
            value if let Some(value) = value.strip_prefix("--reason=") => {
                lock_reason = Some(value.to_string());
            }
            "--no-reason" => lock_reason = None,
            "-b" | "-B" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return worktree_add_usage();
                };
                if arg == "-B" {
                    new_branch_force = Some(value.clone());
                } else {
                    new_branch = Some(value.clone());
                }
            }
            value if value.starts_with("-b") && value.len() > 2 => {
                new_branch = Some(value[2..].to_string());
            }
            value if value.starts_with("-B") && value.len() > 2 => {
                new_branch_force = Some(value[2..].to_string());
            }
            "--guess-remote" => guess_remote = true,
            "--no-guess-remote" => guess_remote = false,
            "--track" => track = Some(true),
            value if value.starts_with("--track=") => track = Some(true),
            "--no-track" => track = Some(false),
            "--relative-paths" => relative_paths = Some(true),
            "--no-relative-paths" => relative_paths = Some(false),
            value if value.starts_with('-') && value != "-" => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return worktree_add_usage();
            }
            value => paths.push(value.to_string()),
        }
        index += 1;
    }

    // Mirror git's argument-validation order exactly (builtin/worktree.c add()).
    if (detach as usize) + (new_branch.is_some() as usize) + (new_branch_force.is_some() as usize)
        > 1
    {
        eprintln!("fatal: options '-b', '-B', and '--detach' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if detach && orphan {
        eprintln!("fatal: options '--orphan' and '--detach' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if orphan && track.is_some() {
        eprintln!("fatal: options '--orphan' and '--track' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if orphan && !checkout {
        eprintln!("fatal: options '--orphan' and '--no-checkout' cannot be used together");
        return Err(GitError::Exit(128));
    }
    // `--orphan` with an explicit commit-ish (two positionals) is illegal.
    if orphan && paths.len() == 2 {
        eprintln!("fatal: option '--orphan' and commit-ish cannot be used together");
        return Err(GitError::Exit(128));
    }
    if lock_reason.is_some() && !keep_locked {
        eprintln!("fatal: the option '--reason' requires '--lock'");
        return Err(GitError::Exit(128));
    }

    if paths.is_empty() || paths.len() > 2 {
        return worktree_add_usage();
    }

    let force_branch = new_branch_force.is_some();
    let branch = new_branch_force.or(new_branch);
    Ok(WorktreeAddOptions {
        force,
        quiet,
        detach,
        checkout,
        lock: keep_locked,
        lock_reason,
        branch,
        force_branch,
        orphan,
        guess_remote,
        track,
        relative_paths,
        path: paths.remove(0),
        start: paths.pop(),
    })
}

fn worktree_add_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git worktree add [-f] [--detach] [--checkout] [--lock [--reason <string>]]\n                        [--orphan] [(-b | -B) <new-branch>] <path> [<commit-ish>]"
    );
    eprintln!();
    eprintln!(
        "    -f, --[no-]force      checkout <branch> even if already checked out in other worktree"
    );
    eprintln!("    -b <branch>           create a new branch");
    eprintln!("    -B <branch>           create or reset a branch");
    eprintln!("    --[no-]orphan         create unborn branch");
    eprintln!("    -d, --[no-]detach     detach HEAD at named commit");
    eprintln!("    --[no-]checkout       populate the new working tree");
    eprintln!("    --[no-]lock           keep the new working tree locked");
    eprintln!("    --[no-]reason <string>");
    eprintln!("                          reason for locking");
    eprintln!("    -q, --[no-]quiet      suppress progress reporting");
    eprintln!("    --[no-]track          set up tracking mode (see git-branch(1))");
    eprintln!(
        "    --[no-]guess-remote   try to match the new branch name with a remote-tracking branch"
    );
    eprintln!("    --[no-]relative-paths use relative paths for worktrees");
    eprintln!();
    Err(GitError::Exit(129))
}

fn collect_worktree_list_entries(
    common_git_dir: &Path,
    format: ObjectFormat,
    expire: bool,
) -> Result<Vec<WorktreeListEntry>> {
    let mut entries = Vec::new();
    let main_path = worktree_root_for_git_dir(common_git_dir)?;
    entries.push(read_worktree_list_entry(
        common_git_dir,
        common_git_dir,
        format,
        main_path,
        None,
        None,
    )?);

    for admin in collect_linked_worktree_admins(common_git_dir)? {
        let prunable_reason = expire.then_some(admin.prunable_reason).flatten();
        entries.push(read_worktree_list_entry(
            &admin.admin_dir,
            common_git_dir,
            format,
            admin.path,
            prunable_reason,
            admin.locked_reason,
        )?);
    }
    Ok(entries)
}

fn collect_linked_worktree_admins(common_git_dir: &Path) -> Result<Vec<LinkedWorktreeAdmin>> {
    let worktrees_dir = common_git_dir.join("worktrees");
    let Ok(admin_entries) = fs::read_dir(&worktrees_dir) else {
        return Ok(Vec::new());
    };
    let mut admins = Vec::new();
    for admin_entry in admin_entries {
        let admin_entry = admin_entry?;
        let admin_dir = admin_entry.path();
        if !admin_dir.is_dir() {
            continue;
        }
        let Some(admin) = linked_worktree_admin(&admin_dir)? else {
            continue;
        };
        admins.push(admin);
    }
    admins.sort_by(|left, right| left.admin_dir.cmp(&right.admin_dir));
    Ok(admins)
}

fn linked_worktree_admin(admin_dir: &Path) -> Result<Option<LinkedWorktreeAdmin>> {
    let gitdir_file = admin_dir.join("gitdir");
    if !gitdir_file.is_file() {
        return Ok(None);
    }
    let value = fs::read_to_string(gitdir_file)?;
    let gitdir = resolve_admin_path(admin_dir, value.trim());
    let Some(path) = gitdir.parent() else {
        return Ok(None);
    };
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let locked_reason = read_worktree_lock_reason(admin_dir)?;
    let prunable_reason = (locked_reason.is_none() && !gitdir.exists())
        .then(|| "gitdir file points to non-existent location".to_string());
    let admin_name = admin_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Some(LinkedWorktreeAdmin {
        admin_dir: admin_dir.to_path_buf(),
        admin_name,
        path,
        prunable_reason,
        locked_reason,
    }))
}

fn resolve_admin_path(admin_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        admin_dir.join(path)
    }
}

fn read_worktree_lock_reason(admin_dir: &Path) -> Result<Option<String>> {
    let locked = admin_dir.join("locked");
    if !locked.is_file() {
        return Ok(None);
    }
    let value = fs::read_to_string(locked)?;
    Ok(Some(value.trim_end_matches('\n').to_string()))
}

fn find_linked_worktree_admin(
    common_git_dir: &Path,
    cwd: &Path,
    path: &str,
) -> Result<LinkedWorktreeAdmin> {
    let target = resolve_cli_path(cwd, path);
    let canonical_target = fs::canonicalize(&target).ok();
    if let Ok(main) = worktree_root_for_git_dir(common_git_dir)
        && (canonical_target.as_deref() == fs::canonicalize(main).ok().as_deref()
            || target == common_git_dir)
    {
        eprintln!("fatal: The main working tree cannot be locked or unlocked");
        return Err(GitError::Exit(128));
    }
    for admin in collect_linked_worktree_admins(common_git_dir)? {
        if canonical_target
            .as_ref()
            .is_some_and(|target| fs::canonicalize(&admin.path).ok().as_ref() == Some(target))
            || normalize_lexical_path(&target) == normalize_lexical_path(&admin.path)
        {
            return Ok(admin);
        }
    }
    eprintln!("fatal: '{path}' is not a working tree");
    Err(GitError::Exit(128))
}

fn find_linked_worktree_admin_for_remove(
    common_git_dir: &Path,
    cwd: &Path,
    path: &str,
) -> Result<LinkedWorktreeAdmin> {
    let target = resolve_cli_path(cwd, path);
    let canonical_target = fs::canonicalize(&target).ok();
    if let Ok(main) = worktree_root_for_git_dir(common_git_dir)
        && canonical_target.as_deref() == fs::canonicalize(main).ok().as_deref()
    {
        eprintln!("fatal: '{path}' is a main working tree");
        return Err(GitError::Exit(128));
    }
    for admin in collect_linked_worktree_admins(common_git_dir)? {
        if canonical_target
            .as_ref()
            .is_some_and(|target| fs::canonicalize(&admin.path).ok().as_ref() == Some(target))
            || normalize_lexical_path(&target) == normalize_lexical_path(&admin.path)
        {
            return Ok(admin);
        }
    }
    eprintln!("fatal: '{path}' is not a working tree");
    Err(GitError::Exit(128))
}

fn find_linked_worktree_admin_for_move(
    common_git_dir: &Path,
    cwd: &Path,
    path: &str,
) -> Result<LinkedWorktreeAdmin> {
    let target = resolve_cli_path(cwd, path);
    let canonical_target = fs::canonicalize(&target).ok();
    if let Ok(main) = worktree_root_for_git_dir(common_git_dir)
        && canonical_target.as_deref() == fs::canonicalize(main).ok().as_deref()
    {
        eprintln!("fatal: '{path}' is a main working tree");
        return Err(GitError::Exit(128));
    }
    for admin in collect_linked_worktree_admins(common_git_dir)? {
        if canonical_target
            .as_ref()
            .is_some_and(|target| fs::canonicalize(&admin.path).ok().as_ref() == Some(target))
            || normalize_lexical_path(&target) == normalize_lexical_path(&admin.path)
        {
            return Ok(admin);
        }
    }
    eprintln!("fatal: '{path}' is not a working tree");
    Err(GitError::Exit(128))
}

fn worktree_move_destination(cwd: &Path, source: &Path, destination: &str) -> Result<PathBuf> {
    let resolved = resolve_cli_path(cwd, destination);
    if resolved.is_file() {
        eprintln!("fatal: '{destination}' already exists");
        return Err(GitError::Exit(128));
    }
    if resolved.is_dir() {
        let Some(name) = source.file_name() else {
            return Err(GitError::InvalidPath(format!(
                "invalid worktree path {}",
                source.display()
            )));
        };
        return Ok(resolved.join(name));
    }
    Ok(resolved)
}

fn repair_worktree_path(common_git_dir: &Path, path: &Path, original: Option<&str>) -> Result<()> {
    let dot_git = path.join(".git");
    if !dot_git.is_file() {
        if let Some(original) = original {
            eprintln!("error: not a valid path: {original}");
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    let Some(admin_dir) = read_gitdir_file(&dot_git)? else {
        if let Some(original) = original {
            eprintln!("error: not a valid path: {original}");
            return Err(GitError::Exit(1));
        }
        return Ok(());
    };
    if !admin_dir.starts_with(common_git_dir.join("worktrees")) || !admin_dir.is_dir() {
        if let Some(original) = original {
            eprintln!("error: not a valid path: {original}");
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    let gitdir_file = admin_dir.join("gitdir");
    let desired = format!("{}\n", dot_git.display());
    let current = fs::read_to_string(&gitdir_file).unwrap_or_default();
    if current != desired {
        eprintln!("repair: gitdir incorrect: {}", gitdir_file.display());
        fs::write(gitdir_file, desired)?;
    }
    Ok(())
}

#[derive(Debug)]
struct WorktreeAddHead {
    branch_name: Option<String>,
    oid: ObjectId,
    prepare_message: String,
    /// Set when the worktree checks out an unborn branch — either explicit
    /// `--orphan` or the DWIM-inferred orphan (repository has no usable local
    /// refs). In this mode there is no source commit, so [`Self::oid`] is
    /// meaningless and the admin dir is laid out like git's orphan worktree
    /// (symref HEAD, empty index, no `ORIG_HEAD`).
    orphan: bool,
}

/// Mirrors git's `can_use_local_refs` (builtin/worktree.c): the repository has a
/// usable local ref when HEAD resolves to a real object, or any branch ref
/// exists. When only branches exist but HEAD is invalid (a dangling/orphan
/// HEAD), git additionally warns "HEAD points to an invalid (or orphaned)
/// reference." unless `--quiet`. Returns whether a local ref is usable.
fn can_use_local_refs(
    common_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    quiet: bool,
) -> Result<bool> {
    if resolve_revision(common_git_dir, format, "HEAD").is_ok() {
        return Ok(true);
    }
    for reference in store.list_refs()? {
        if reference.name.starts_with("refs/heads/") {
            if !quiet {
                eprintln!("warning: HEAD points to an invalid (or orphaned) reference.");
            }
            return Ok(true);
        }
    }
    Ok(false)
}

/// Mirrors git's `can_use_remote_refs` (builtin/worktree.c). Returns whether a
/// remote-tracking ref can supply the source. When `--guess-remote` is set but
/// no remote ref exists yet a remote IS configured (and no `-f`), git dies
/// asking the user to fetch first — sley cannot fetch, so this is the terminal
/// outcome for those cells.
fn can_use_remote_refs(
    common_git_dir: &Path,
    store: &FileRefStore,
    options: &WorktreeAddOptions,
) -> Result<bool> {
    if !options.guess_remote {
        return Ok(false);
    }
    for reference in store.list_refs()? {
        if reference.name.starts_with("refs/remotes/") {
            return Ok(true);
        }
    }
    if options.force == 0 {
        let config = GitConfig::read(common_git_dir.join("config")).unwrap_or_default();
        if !sley_config::remotes::remote_names(&config).is_empty() {
            eprintln!(
                "fatal: No local or remote refs exist despite at least one remote\npresent, stopping; use 'add -f' to override or fetch a remote first"
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(false)
}

/// Mirrors git's `dwim_orphan` (builtin/worktree.c): decides whether
/// `worktree add` should infer `--orphan`. Returns `true` when neither a local
/// nor (when `remote`) a remote ref can supply a source. When inferring, git
/// prints the "No possible source branch, inferring '--orphan'" line (unless
/// `--quiet`) and then dies if `--track`/`--no-checkout` make the inferred
/// orphan an illegal combination.
fn dwim_orphan(
    common_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: &WorktreeAddOptions,
    remote: bool,
) -> Result<bool> {
    if can_use_local_refs(common_git_dir, format, store, options.quiet)? {
        return Ok(false);
    }
    if remote && can_use_remote_refs(common_git_dir, store, options)? {
        return Ok(false);
    }
    if !options.quiet {
        eprintln!("No possible source branch, inferring '--orphan'");
    }
    // git checks --track before --no-checkout.
    if options.track.is_some() {
        eprintln!("fatal: options '--orphan' and '--track' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if !options.checkout {
        eprintln!("fatal: options '--orphan' and '--no-checkout' cannot be used together");
        return Err(GitError::Exit(128));
    }
    Ok(true)
}

/// Resolve the commit-ish a `worktree add` should start from. Maps the `-`
/// shorthand to `@{-1}` (git's "previous checkout"). Returns the resolved
/// commit-ish string to feed the rest of the flow.
fn worktree_add_start_commitish(options: &WorktreeAddOptions) -> String {
    let start = options.start.as_deref().unwrap_or("HEAD");
    if start == "-" {
        "@{-1}".to_string()
    } else {
        start.to_string()
    }
}

/// Returns the branch shortname `commitish` refers to when it names an existing
/// local branch — directly (`refs/heads/<commitish>` exists) or via `@{-N}`
/// (the N-th previously checked-out branch). Returns `None` for a non-branch
/// commit-ish (so the caller checks out a detached HEAD).
fn worktree_add_branch_for_commitish(
    store: &FileRefStore,
    commitish: &str,
) -> Result<Option<String>> {
    if let Some(rest) = commitish.strip_prefix("@{-") {
        if let Some(num) = rest.strip_suffix('}')
            && let Ok(n) = num.parse::<usize>()
            && n > 0
            && let Some(name) = previous_checkout_branch(store, n)?
        {
            let refname = branch_ref_name(&name)?;
            if store.read_ref(&refname)?.is_some() {
                return Ok(Some(name));
            }
        }
        return Ok(None);
    }
    let Ok(refname) = branch_ref_name(commitish) else {
        return Ok(None);
    };
    if store.read_ref(&refname)?.is_some() {
        return Ok(Some(commitish.to_string()));
    }
    Ok(None)
}

/// Resolve `@{-N}` to the branch shortname it names by scanning HEAD's reflog
/// newest-first for "checkout: moving from X to Y" entries (mirrors
/// sley-rev's resolution but yields the branch *name*, which `worktree add`
/// needs to point the new worktree's HEAD at the branch).
fn previous_checkout_branch(store: &FileRefStore, n: usize) -> Result<Option<String>> {
    let entries = store.read_reflog("HEAD")?;
    let mut seen = 0usize;
    for entry in entries.iter().rev() {
        let Ok(message) = std::str::from_utf8(&entry.message) else {
            continue;
        };
        let Some(rest) = message.strip_prefix("checkout: moving from ") else {
            continue;
        };
        let Some((from, _to)) = rest.rsplit_once(" to ") else {
            continue;
        };
        seen += 1;
        if seen == n {
            return Ok(Some(from.to_string()));
        }
    }
    Ok(None)
}

fn worktree_add_resolve_head(
    common_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    path: &Path,
    options: &WorktreeAddOptions,
    committer: Vec<u8>,
) -> Result<WorktreeAddHead> {
    // Explicit `--orphan` (the branch name was filled in by the caller): the new
    // worktree checks out an unborn branch with no source commit.
    if options.orphan {
        let branch = options
            .branch
            .clone()
            .unwrap_or(default_worktree_add_branch_name(path)?);
        let refname = branch_ref_name(&branch)?;
        if store.read_ref(&refname)?.is_some() {
            eprintln!("fatal: a branch named '{branch}' already exists");
            return Err(GitError::Exit(128));
        }
        return Ok(WorktreeAddHead {
            branch_name: Some(branch.clone()),
            oid: ObjectId::empty_tree(format),
            prepare_message: format!("Preparing worktree (new branch '{branch}')"),
            orphan: true,
        });
    }

    if let Some(branch) = options.branch.as_ref() {
        // DWIM: `worktree add -b <branch> <path>` with no explicit commit-ish in
        // a repo with no usable local refs infers `--orphan` (git
        // builtin/worktree.c `add`: the `ac < 2 && new_branch` arm).
        if options.start.is_none()
            && !options.force_branch
            && dwim_orphan(common_git_dir, format, store, options, false)?
        {
            return worktree_add_inferred_orphan_head(branch.clone(), format);
        }
        let start = options.start.as_deref().unwrap_or("HEAD");
        // git: `if (!opts.orphan && !lookup_commit_reference_by_name(branch))`
        // — when the start point is unresolvable (e.g. a dangling HEAD), emit
        // the invalid-reference error (with the `-b`-aware orphan hint) instead
        // of a lower-level failure from branch creation.
        worktree_add_resolve_commitish(common_git_dir, format, options, start)?;
        let was_reset = checkout_create_or_reset_branch(
            common_git_dir,
            format,
            branch,
            start,
            options.force_branch,
            committer,
        )?;
        let oid = resolve_revision(common_git_dir, format, branch)?;
        let prepare_message = if was_reset {
            format!("Preparing worktree (resetting branch '{branch}'; was at {oid})")
        } else {
            format!("Preparing worktree (new branch '{branch}')")
        };
        return Ok(WorktreeAddHead {
            branch_name: Some(branch.clone()),
            oid,
            prepare_message,
            orphan: false,
        });
    }

    let commitish = worktree_add_start_commitish(options);

    if options.detach {
        // Detaching at HEAD in a repo whose HEAD is dangling warns + dies via
        // can_use_local_refs (git: the `detach && branch == "HEAD"` arm).
        if commitish == "HEAD" {
            can_use_local_refs(common_git_dir, format, store, options.quiet)?;
        }
        let oid = worktree_add_resolve_commitish(common_git_dir, format, options, &commitish)?;
        return Ok(WorktreeAddHead {
            branch_name: None,
            oid,
            prepare_message: format!(
                "Preparing worktree (detached HEAD {})",
                format_log_abbrev_oid(&oid)
            ),
            orphan: false,
        });
    }

    if options.start.is_some() {
        // `ac == 2`: an explicit commit-ish. If it names a branch, check it out
        // (HEAD becomes a symref); otherwise detach.
        if let Some(branch) = worktree_add_branch_for_commitish(store, &commitish)? {
            let oid = resolve_revision(common_git_dir, format, &commitish)?;
            return Ok(WorktreeAddHead {
                branch_name: Some(branch),
                oid,
                prepare_message: format!("Preparing worktree (checking out '{commitish}')"),
                orphan: false,
            });
        }
        let oid = worktree_add_resolve_commitish(common_git_dir, format, options, &commitish)?;
        return Ok(WorktreeAddHead {
            branch_name: None,
            oid,
            prepare_message: format!(
                "Preparing worktree (detached HEAD {})",
                format_log_abbrev_oid(&oid)
            ),
            orphan: false,
        });
    }

    // `ac < 2` plain DWIM: a worktree-named branch already present is checked
    // out; otherwise a new branch is created from HEAD, or `--orphan` is
    // inferred when the repo has no usable local/remote refs.
    let branch = default_worktree_add_branch_name(path)?;
    let branch_ref = branch_ref_name(&branch)?;
    if store.read_ref(&branch_ref)?.is_some() {
        let oid = resolve_revision(common_git_dir, format, &branch)?;
        let prepare_message = format!("Preparing worktree (checking out '{branch}')");
        return Ok(WorktreeAddHead {
            branch_name: Some(branch),
            oid,
            prepare_message,
            orphan: false,
        });
    }
    if dwim_orphan(common_git_dir, format, store, options, true)? {
        return worktree_add_inferred_orphan_head(branch, format);
    }
    // The new branch is created from HEAD. git's `!opts.orphan &&
    // !lookup_commit_reference_by_name("HEAD")` arm emits the invalid-reference
    // error (with the no-`-b` orphan hint) when HEAD is a dangling reference.
    worktree_add_resolve_commitish(common_git_dir, format, options, "HEAD")?;
    commands::branch::create_branch_from_start(common_git_dir, format, store, &branch, None)?;
    let oid = resolve_revision(common_git_dir, format, &branch)?;
    Ok(WorktreeAddHead {
        branch_name: Some(branch.clone()),
        oid,
        prepare_message: format!("Preparing worktree (new branch '{branch}')"),
        orphan: false,
    })
}

/// Resolve an explicit commit-ish to its object id, emitting git's
/// invalid-reference error (and, when DWIM-eligible, the orphan hint) on
/// failure. Mirrors the `!opts.orphan && !lookup_commit_reference_by_name`
/// arm: the hint fires only when there was no explicit commit-ish (`ac < 2`)
/// and `--quiet` is off, and uses the `-b`/`-B`-aware text when a new-branch
/// option was supplied.
fn worktree_add_resolve_commitish(
    common_git_dir: &Path,
    format: ObjectFormat,
    options: &WorktreeAddOptions,
    commitish: &str,
) -> Result<ObjectId> {
    match resolve_revision(common_git_dir, format, commitish) {
        Ok(oid) => Ok(oid),
        Err(_) => {
            let attempt_hint = !options.quiet && options.start.is_none();
            if attempt_hint {
                eprintln!("hint: If you meant to create a worktree containing a new unborn branch");
                eprintln!("hint: (branch with no commits) for this repository, you can do so");
                eprintln!("hint: using the --orphan flag:");
                eprintln!("hint:");
                if let Some(branch) = options.branch.as_ref() {
                    eprintln!(
                        "hint:     git worktree add --orphan -b {} {}",
                        branch, options.path
                    );
                } else {
                    eprintln!("hint:     git worktree add --orphan {}", options.path);
                }
                eprintln!("hint:");
                eprintln!(
                    "hint: Disable this message with \"git config set advice.worktreeAddOrphan false\""
                );
            }
            eprintln!("fatal: invalid reference: {commitish}");
            Err(GitError::Exit(128))
        }
    }
}

/// Builds the [`WorktreeAddHead`] for a DWIM-inferred `--orphan` `worktree add`.
/// The "No possible source branch, inferring '--orphan'" line (and any illegal
/// `--track`/`--no-checkout` combo) was already emitted by [`dwim_orphan`]; here
/// we only carry the "Preparing worktree (new branch '<branch>')" line.
fn worktree_add_inferred_orphan_head(
    branch: String,
    format: ObjectFormat,
) -> Result<WorktreeAddHead> {
    Ok(WorktreeAddHead {
        branch_name: Some(branch.clone()),
        oid: ObjectId::empty_tree(format),
        prepare_message: format!("Preparing worktree (new branch '{branch}')"),
        orphan: true,
    })
}

fn default_worktree_add_branch_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
        .ok_or_else(|| GitError::InvalidPath(format!("invalid worktree path {}", path.display())))
}

/// Port of git's `check_candidate_path` (builtin/worktree.c): when the
/// destination path is already a *registered* worktree (its admin dir exists
/// even though the directory on disk may be gone), `worktree add` must refuse
/// unless forced. A locked registered worktree needs `-f -f`; an unlocked one
/// needs `-f`. With sufficient force we delete the stale admin dir so the add
/// can re-register the path.
fn check_worktree_candidate_path(
    common_git_dir: &Path,
    path: &Path,
    original: &str,
    force: usize,
) -> Result<()> {
    let canonical = fs::canonicalize(path).ok();
    for admin in collect_linked_worktree_admins(common_git_dir)? {
        let matches = match (&canonical, fs::canonicalize(&admin.path).ok()) {
            (Some(a), Some(b)) => *a == b,
            _ => normalize_lexical_path(path) == normalize_lexical_path(&admin.path),
        };
        if !matches {
            continue;
        }
        let locked = admin.locked_reason.is_some();
        if (!locked && force >= 1) || (locked && force >= 2) {
            fs::remove_dir_all(&admin.admin_dir)?;
            return Ok(());
        }
        if locked {
            eprintln!(
                "fatal: '{original}' is a missing but locked worktree;\nuse 'add -f -f' to override, or 'unlock' and 'prune' or 'remove' to clear"
            );
        } else {
            eprintln!(
                "fatal: '{original}' is a missing but already registered worktree;\nuse 'add -f' to override, or 'prune' or 'remove' to clear"
            );
        }
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// Port of git's `write_worktree_linking_files` (worktree.c): write the
/// worktree's `.git` link file and the admin dir's `gitdir` file. With
/// `relative_paths`, both are expressed relative to each other's real path and
/// the repository is upgraded to format 1 with `extensions.relativeWorktrees`;
/// otherwise both hold absolute, symlink-resolved paths (the historical default).
fn write_worktree_linking_files(
    common_git_dir: &Path,
    admin_dir: &Path,
    worktree_path: &Path,
    relative_paths: bool,
) -> Result<()> {
    let dotgit = worktree_path.join(".git");
    if relative_paths {
        upgrade_repo_for_relative_worktrees(common_git_dir)?;
        // git canonicalizes both real paths before computing the relatives.
        let real_wt =
            fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
        let real_admin = fs::canonicalize(admin_dir).unwrap_or_else(|_| admin_dir.to_path_buf());
        let admin_to_wt = relative_path_from_absolute_components(&real_admin, &real_wt)?;
        let wt_to_admin = relative_path_from_absolute_components(&real_wt, &real_admin)?;
        let admin_to_wt = admin_to_wt.trim_end_matches('/');
        let wt_to_admin = wt_to_admin.trim_end_matches('/');
        fs::write(admin_dir.join("gitdir"), format!("{admin_to_wt}/.git\n"))?;
        fs::write(&dotgit, format!("gitdir: {wt_to_admin}\n"))?;
    } else {
        // git writes the symlink-resolved real paths (`strbuf_realpath`), so a
        // `./` segment in the user-supplied path never leaks into the link files.
        let real_admin = fs::canonicalize(admin_dir).unwrap_or_else(|_| admin_dir.to_path_buf());
        let real_wt =
            fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
        fs::write(&dotgit, format!("gitdir: {}\n", real_admin.display()))?;
        fs::write(
            admin_dir.join("gitdir"),
            format!("{}/.git\n", real_wt.display()),
        )?;
    }
    Ok(())
}

/// Upgrade the repository to format version 1 with the `relativeWorktrees`
/// extension, mirroring git's `upgrade_repository_format(1)` +
/// `extensions.relativeWorktrees=true`. Idempotent: when the extension is
/// already present nothing changes.
fn upgrade_repo_for_relative_worktrees(common_git_dir: &Path) -> Result<()> {
    let config_path = common_git_dir.join("config");
    let mut config = GitConfig::read(&config_path).unwrap_or_default();
    if config
        .get_bool("extensions", None, "relativeWorktrees")
        .unwrap_or(false)
    {
        return Ok(());
    }
    // core.repositoryformatversion = 1
    set_config_simple(&mut config, "core", "repositoryformatversion", "1");
    set_config_simple(&mut config, "extensions", "relativeWorktrees", "true");
    fs::write(&config_path, config.to_canonical_bytes())?;
    Ok(())
}

/// Set a single non-subsectioned `<section>.<key> = <value>`, replacing the
/// last existing value for that key or appending to the (last) section, creating
/// the section if absent.
fn set_config_simple(config: &mut GitConfig, section: &str, key: &str, value: &str) {
    if let Some(existing) = config
        .sections
        .iter_mut()
        .rev()
        .find(|s| s.name.eq_ignore_ascii_case(section) && s.subsection.is_none())
    {
        if let Some(entry) = existing
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.key.eq_ignore_ascii_case(key))
        {
            entry.value = Some(value.to_string());
        } else {
            existing
                .entries
                .push(sley_config::ConfigEntry::new(key, Some(value.to_string())));
        }
        return;
    }
    config.sections.push(sley_config::ConfigSection::new(
        section,
        None,
        vec![sley_config::ConfigEntry::new(key, Some(value.to_string()))],
    ));
}

fn validate_worktree_add_destination(path: &Path, original: &str) -> Result<()> {
    if path.is_file() {
        eprintln!("fatal: '{original}' already exists");
        return Err(GitError::Exit(128));
    }
    if path.is_dir() && fs::read_dir(path)?.next().is_some() {
        eprintln!("fatal: '{original}' already exists");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// git's `refname_disposition` table (refs.c): per-byte classification used to
/// sanitize a single refname component. 0 = allowed, 1 = terminator (`\0`/`/`),
/// 2 = `.`, 3 = `{`, 4 = forbidden, 5 = `*`.
const REFNAME_DISPOSITION: [u8; 256] = [
    1, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, //
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, //
    4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 2, 1, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 4, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 0, 4, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 4, 4, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
];

const LOCK_SUFFIX: &[u8] = b".lock";

/// Port of git's single-component `sanitize_refname_component`
/// (`check_or_sanitize_refname` + `check_refname_component` with
/// `REFNAME_ALLOW_ONELEVEL`): forbidden bytes (and `*`, `@{`) become `-`, ".."
/// collapses to ".", a leading "." becomes "-", and trailing ".lock" suffixes
/// are stripped. Operates on bytes (refnames are byte strings) and returns a
/// lossy UTF-8 string for use as a directory name.
fn sanitize_refname_component(input: &str) -> String {
    let bytes = input.as_bytes();
    if bytes == b"@" {
        return "-".to_string();
    }
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut last: u8 = 0;
    for &ch in bytes {
        let disp = REFNAME_DISPOSITION[ch as usize];
        if disp != 1 {
            out.push(ch);
        }
        match disp {
            1 => break, // terminator (no interior '/' in a basename, but be safe)
            2 => {
                if last == b'.' {
                    // collapse ".." to a single "."
                    out.pop();
                }
            }
            3 => {
                if last == b'@' {
                    // "@{" -> "@-" (replace the just-pushed '{')
                    let n = out.len();
                    out[n - 1] = b'-';
                }
            }
            4 | 5 => {
                // forbidden char (and '*' outside a refspec pattern) -> '-'
                let n = out.len();
                out[n - 1] = b'-';
            }
            _ => {}
        }
        last = ch;
    }
    if out.first() == Some(&b'.') {
        out[0] = b'-';
    }
    while out.len() >= LOCK_SUFFIX.len() && out.ends_with(LOCK_SUFFIX) {
        out.truncate(out.len() - LOCK_SUFFIX.len());
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn create_linked_worktree_admin_dir(common_git_dir: &Path, path: &Path) -> Result<PathBuf> {
    let worktrees_dir = common_git_dir.join("worktrees");
    fs::create_dir_all(&worktrees_dir)?;
    let raw = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("worktree");
    // git derives the admin name via `sanitize_refname_component` (refs.c), not
    // a naive '/'->'-' map: forbidden chars become '-', "@{" / leading '.'
    // become '-', ".." collapses, and trailing ".lock"s are stripped.
    let base = sanitize_refname_component(raw);
    let base = if base.is_empty() {
        "worktree".to_string()
    } else {
        base
    };
    for suffix in 0..1000 {
        let name = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}{suffix}")
        };
        let admin_dir = worktrees_dir.join(name);
        if !admin_dir.exists() {
            fs::create_dir(&admin_dir)?;
            return Ok(admin_dir);
        }
    }
    Err(GitError::Transaction(
        "unable to allocate linked worktree admin directory".into(),
    ))
}

fn branch_checked_out_worktree(
    common_git_dir: &Path,
    refname: &str,
    ignore_path: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let is_ignored = |path: &Path| {
        ignore_path.is_some_and(|ignore| {
            fs::canonicalize(path).ok().as_ref() == fs::canonicalize(ignore).ok().as_ref()
                || normalize_lexical_path(path) == normalize_lexical_path(ignore)
        })
    };
    if worktree_head_points_to(common_git_dir, refname)?
        && let Ok(path) = worktree_root_for_git_dir(common_git_dir)
        && !is_ignored(&path)
    {
        return Ok(Some(fs::canonicalize(&path).unwrap_or(path)));
    }
    for admin in collect_linked_worktree_admins(common_git_dir)? {
        if is_ignored(&admin.path) {
            continue;
        }
        if worktree_head_points_to(&admin.admin_dir, refname)? {
            return Ok(Some(
                fs::canonicalize(&admin.path).unwrap_or(admin.path.to_path_buf()),
            ));
        }
    }
    Ok(None)
}

fn worktree_head_points_to(git_dir: &Path, refname: &str) -> Result<bool> {
    let head = fs::read_to_string(git_dir.join("HEAD"))?;
    Ok(head.trim() == format!("ref: {refname}"))
}

fn write_linked_worktree_checkout(
    common_git_dir: &Path,
    admin_dir: &Path,
    worktree_path: &Path,
    format: ObjectFormat,
    target_oid: &ObjectId,
    checkout: bool,
) -> Result<()> {
    let mut index_entries = Vec::new();
    if checkout {
        let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
        let object = db.read_object(target_oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {}, found {}",
                target_oid,
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse_ref(format, &object.body)?;
        let mut entries = BTreeMap::new();
        collect_worktree_head_tree_entries(&db, format, &commit.tree, Vec::new(), &mut entries)?;
        for (path, entry) in entries {
            if entry.mode == 0o160000 {
                index_entries.push(worktree_index_entry(path, entry.oid, entry.mode, 0, None));
                continue;
            }
            let blob = db.read_object(&entry.oid)?;
            if blob.object_type != ObjectType::Blob {
                return Err(GitError::InvalidObject(format!(
                    "expected blob {}, found {}",
                    entry.oid,
                    blob.object_type.as_str()
                )));
            }
            let file_path = worktree_path.join(String::from_utf8_lossy(&path).as_ref());
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&file_path, &blob.body)?;
            let metadata = fs::metadata(&file_path)?;
            index_entries.push(worktree_index_entry(
                path,
                entry.oid,
                entry.mode,
                metadata.len(),
                metadata.modified().ok(),
            ));
        }
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        sley_worktree::repository_index_path(admin_dir),
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(())
}

/// Writes the empty index of an inferred-orphan worktree, byte-for-byte like
/// git: a zero-entry v2 index whose `TREE` cache-tree caches the empty tree
/// (`entry_count = 0`, oid = the empty tree).
fn write_empty_worktree_index(admin_dir: &Path, format: ObjectFormat) -> Result<()> {
    let mut index = Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    };
    index.set_cache_tree(Some(&sley_index::CacheTree {
        entry_count: 0,
        oid: Some(ObjectId::empty_tree(format)),
        subtrees: Vec::new(),
    }))?;
    fs::write(
        sley_worktree::repository_index_path(admin_dir),
        index.write(format)?,
    )?;
    Ok(())
}

fn worktree_index_entry(
    path: Vec<u8>,
    oid: ObjectId,
    mode: u32,
    size: u64,
    modified: Option<std::time::SystemTime>,
) -> sley_index::IndexEntry {
    let duration = modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();
    let flags = path.len().min(0x0fff) as u16;
    sley_index::IndexEntry {
        ctime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        ctime_nanoseconds: duration.subsec_nanos(),
        mtime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        mtime_nanoseconds: duration.subsec_nanos(),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: size.min(u32::MAX as u64) as u32,
        oid,
        flags,
        flags_extended: 0,
        path: BString::from(path),
    }
}

fn worktree_remove_has_local_changes(
    common_git_dir: &Path,
    admin: &LinkedWorktreeAdmin,
    format: ObjectFormat,
) -> Result<bool> {
    let index = read_repository_index(&admin.admin_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });
    let head = worktree_head_tree_entries(common_git_dir, &admin.admin_dir, format)?;
    let mut tracked = BTreeSet::new();
    for entry in &index.entries {
        tracked.insert(String::from_utf8_lossy(&entry.path).into_owned());
        if head
            .get(entry.path.as_bytes())
            .is_none_or(|head_entry| head_entry.mode != entry.mode || head_entry.oid != entry.oid)
        {
            return Ok(true);
        }
        let path = admin
            .path
            .join(String::from_utf8_lossy(&entry.path).as_ref());
        if !path.exists() {
            return Ok(true);
        }
        if entry.mode == 0o100644 || entry.mode == 0o100755 {
            let body = fs::read(&path)?;
            let oid = sley_core::object_id_for_bytes(format, "blob", &body)?;
            if oid != entry.oid {
                return Ok(true);
            }
        }
    }
    if head.len() != index.entries.len() {
        return Ok(true);
    }
    submodule_worktree_has_untracked_entries(&admin.path, &admin.path, &tracked)
}

fn worktree_head_tree_entries(
    common_git_dir: &Path,
    admin_git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, WorktreeTrackedEntry>> {
    let Some(head_oid) = worktree_head_oid(common_git_dir, admin_git_dir, format)? else {
        return Ok(BTreeMap::new());
    };
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let object = db.read_object(&head_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "HEAD {head_oid} is not a commit"
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let mut entries = BTreeMap::new();
    collect_worktree_head_tree_entries(&db, format, &commit.tree, Vec::new(), &mut entries)?;
    Ok(entries)
}

fn worktree_head_oid(
    common_git_dir: &Path,
    admin_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<ObjectId>> {
    let head = fs::read_to_string(admin_git_dir.join("HEAD"))?;
    let head = head.trim();
    if let Some(name) = head.strip_prefix("ref: ") {
        return read_common_ref_oid(common_git_dir, format, name);
    }
    Ok(Some(ObjectId::from_hex(format, head)?))
}

fn collect_worktree_head_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    entries: &mut BTreeMap<Vec<u8>, WorktreeTrackedEntry>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {}, found {}",
            tree_oid,
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            collect_worktree_head_tree_entries(db, format, &entry.oid, path, entries)?;
        } else {
            entries.insert(
                path,
                WorktreeTrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            );
        }
    }
    Ok(())
}

fn read_worktree_list_entry(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    path: PathBuf,
    prunable_reason: Option<String>,
    locked_reason: Option<String>,
) -> Result<WorktreeListEntry> {
    let head = fs::read_to_string(git_dir.join("HEAD"))?;
    let head = head.trim();
    let (head, branch, detached) = if let Some(branch) = head.strip_prefix("ref: ") {
        let oid = read_common_ref_oid(common_git_dir, format, branch)?.unwrap_or(zero_oid(format)?);
        (oid, Some(branch.to_string()), false)
    } else {
        (ObjectId::from_hex(format, head)?, None, true)
    };
    Ok(WorktreeListEntry {
        path: path.display().to_string(),
        head,
        branch,
        detached,
        prunable_reason,
        locked_reason,
    })
}

fn read_common_ref_oid(
    git_dir: &Path,
    format: ObjectFormat,
    name: &str,
) -> Result<Option<ObjectId>> {
    let store = FileRefStore::new(git_dir, format);
    let mut current = name.to_string();
    for _ in 0..8 {
        match store.read_ref(&current)? {
            Some(RefTarget::Direct(oid)) => return Ok(Some(oid)),
            Some(RefTarget::Symbolic(target)) => current = target,
            None => return Ok(None),
        }
    }
    Err(GitError::InvalidFormat(format!(
        "symbolic ref loop while resolving {name}"
    )))
}

fn print_worktree_list_default(entries: &[WorktreeListEntry], verbose: bool) {
    let width = entries
        .iter()
        .map(|entry| entry.path.chars().count())
        .max()
        .unwrap_or(0);
    for entry in entries {
        let label = if let Some(branch) = &entry.branch {
            branch
                .strip_prefix("refs/heads/")
                .map(|name| format!("[{name}]"))
                .unwrap_or_else(|| format!("[{branch}]"))
        } else if entry.detached {
            "(detached HEAD)".to_string()
        } else {
            String::new()
        };
        let locked = entry
            .locked_reason
            .as_ref()
            .is_some_and(|reason| !verbose || reason.is_empty());
        let prunable = entry.prunable_reason.is_some() && !verbose && !locked;
        if locked {
            println!(
                "{:<width$} {} {} locked",
                entry.path,
                format_log_abbrev_oid(&entry.head),
                label,
                width = width
            );
        } else if prunable {
            println!(
                "{:<width$} {} {} prunable",
                entry.path,
                format_log_abbrev_oid(&entry.head),
                label,
                width = width
            );
        } else {
            println!(
                "{:<width$} {} {}",
                entry.path,
                format_log_abbrev_oid(&entry.head),
                label,
                width = width
            );
        }
        if verbose
            && let Some(reason) = &entry.locked_reason
            && !reason.is_empty()
        {
            println!("\tlocked: {reason}");
        }
        if verbose && let Some(reason) = &entry.prunable_reason {
            println!("\tprunable: {reason}");
        }
    }
}

fn print_worktree_list_porcelain(entries: &[WorktreeListEntry], z: bool) -> Result<()> {
    let mut stdout = io::stdout();
    let separator = if z { b"\0" as &[u8] } else { b"\n" as &[u8] };
    for entry in entries {
        stdout.write_all(b"worktree ")?;
        stdout.write_all(entry.path.as_bytes())?;
        stdout.write_all(separator)?;
        stdout.write_all(b"HEAD ")?;
        stdout.write_all(entry.head.to_hex().as_bytes())?;
        stdout.write_all(separator)?;
        if let Some(branch) = &entry.branch {
            stdout.write_all(b"branch ")?;
            stdout.write_all(branch.as_bytes())?;
            stdout.write_all(separator)?;
        } else if entry.detached {
            stdout.write_all(b"detached")?;
            stdout.write_all(separator)?;
        }
        if let Some(reason) = &entry.prunable_reason {
            stdout.write_all(b"prunable ")?;
            stdout.write_all(reason.as_bytes())?;
            stdout.write_all(separator)?;
        }
        if let Some(reason) = &entry.locked_reason {
            stdout.write_all(b"locked")?;
            if !reason.is_empty() {
                stdout.write_all(b" ")?;
                stdout.write_all(reason.as_bytes())?;
            }
            stdout.write_all(separator)?;
        }
        stdout.write_all(separator)?;
    }
    Ok(())
}
