//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_submodule(args: &[String]) -> Result<()> {
    let mut index = 0;
    let mut quiet = false;
    let mut leading = Vec::new();
    loop {
        match args.get(index).map(String::as_str) {
            Some("--quiet" | "-q") => {
                quiet = true;
                index += 1;
            }
            // `--cached` may precede the subcommand (`git submodule --cached
            // status`); remember it for the status default.
            Some("--cached") => {
                leading.push("--cached".to_string());
                index += 1;
            }
            Some("-h") => {
                // `git submodule -h` prints the usage to stdout and succeeds.
                println!("{}", submodule_usage_text());
                return Ok(());
            }
            // A bare `--` / `--end-of-options` (or any other unknown leading
            // option) is a usage error.
            Some("--" | "--end-of-options") if args.len() == index + 1 => {
                return submodule_usage();
            }
            _ => break,
        }
    }
    if matches!(args.get(index).map(String::as_str), Some("status")) {
        index += 1;
        let mut rest = leading.clone();
        rest.extend_from_slice(&args[index..]);
        return cmd_submodule_status(&rest, quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("add")) {
        index += 1;
        return cmd_submodule_add(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("update")) {
        index += 1;
        return cmd_submodule_update(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("init")) {
        index += 1;
        return cmd_submodule_init(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("deinit")) {
        index += 1;
        return cmd_submodule_deinit(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("sync")) {
        index += 1;
        return cmd_submodule_sync(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("absorbgitdirs")) {
        index += 1;
        return cmd_submodule_absorbgitdirs(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("foreach")) {
        index += 1;
        return cmd_submodule_foreach(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("summary")) {
        index += 1;
        return cmd_submodule_summary(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("set-branch")) {
        index += 1;
        return cmd_submodule_set_branch(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("set-url")) {
        index += 1;
        return cmd_submodule_set_url(&args[index..], quiet);
    }
    let mut rest = leading;
    rest.extend_from_slice(&args[index..]);
    cmd_submodule_status(&rest, quiet)
}

#[derive(Debug)]
struct SubmoduleStatusOptions<'a> {
    cached: bool,
    quiet: bool,
    recursive: bool,
    paths: Vec<&'a str>,
}

#[derive(Debug)]
struct SubmoduleConfigEntry {
    name: String,
    path: String,
    url: Option<String>,
    update: Option<String>,
}

#[derive(Debug)]
struct SubmoduleStatusEntry {
    path: String,
    display_path: String,
}

struct SubmoduleAddOptions {
    repository: String,
    path: Option<String>,
    branch: Option<String>,
    name: Option<String>,
    force: bool,
    quiet: bool,
}

struct SubmoduleUpdateOptions<'a> {
    init: bool,
    quiet: bool,
    paths: Vec<&'a str>,
}

fn cmd_submodule_status(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_status_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodules(&cwd, &worktree_root, submodules, &options.paths)?;
    if quiet || options.quiet {
        return Ok(());
    }
    let index = read_repository_index(&git_dir, format)?;
    for submodule in selected {
        print_submodule_status_tree(
            &cwd,
            &worktree_root,
            &index,
            &submodule,
            options.cached,
            options.recursive,
        )?;
    }
    Ok(())
}

fn parse_submodule_status_options(args: &[String]) -> Result<SubmoduleStatusOptions<'_>> {
    let mut cached = false;
    let mut quiet = false;
    let mut recursive = false;
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--cached" => cached = true,
            "--quiet" | "-q" => quiet = true,
            "--recursive" => recursive = true,
            "--no-recursive" => return submodule_usage(),
            value if value.starts_with('-') => {
                return submodule_usage();
            }
            value => paths.push(value),
        }
    }
    Ok(SubmoduleStatusOptions {
        cached,
        quiet,
        recursive,
        paths,
    })
}

fn cmd_submodule_add(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_add_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let path = options
        .path
        .clone()
        .unwrap_or_else(|| default_submodule_path(&options.repository));
    let normalized_path = normalize_submodule_add_path(&cwd, &worktree_root, &path)?;
    let destination = worktree_root.join(&normalized_path);

    let existing_repo =
        destination.is_dir() && sley_diff_merge::gitlink_git_dir(&destination).is_some();
    if existing_repo && submodule_head(&destination).is_err() {
        eprintln!("fatal: '{normalized_path}' does not have a commit checked out");
        return Err(GitError::Exit(128));
    }
    if !options.force
        && let Some(index) = read_repository_index(&git_dir, format)?
        && index
            .entries
            .iter()
            .any(|entry| path_matches_or_is_beneath(&entry.path, normalized_path.as_bytes()))
    {
        eprintln!("fatal: '{normalized_path}' already exists in the index");
        return Err(GitError::Exit(128));
    }
    // Upstream add_submodule(): an existing directory must be a populated
    // (non-bare) repository — anything else, even an empty directory, is fatal.
    if destination.is_dir() && !existing_repo {
        eprintln!("fatal: '{normalized_path}' already exists and is not a valid git repo");
        return Err(GitError::Exit(128));
    }

    if existing_repo {
        println!("Adding existing repo at '{normalized_path}' to the index");
    } else {
        let modules_git_dir = git_dir.join("modules").join(&normalized_path);
        let mut clone_args = Vec::new();
        if options.quiet {
            clone_args.push("-q".to_string());
        }
        if let Some(branch) = &options.branch {
            clone_args.push("-b".to_string());
            clone_args.push(branch.clone());
        }
        clone_args.push("--separate-git-dir".to_string());
        clone_args.push(modules_git_dir.display().to_string());
        clone_args.push(options.repository.clone());
        clone_args.push(destination.display().to_string());
        super::remote_cmds::cmd_clone(&clone_args)?;

        rewrite_submodule_gitdir_file(&destination, &modules_git_dir)?;
        set_submodule_core_worktree(&destination, &modules_git_dir)?;
    }

    let (submodule_git_dir, head_oid) = submodule_head(&destination)?;
    let submodule_format = repository_object_format(&submodule_git_dir)?;
    if submodule_format != format {
        eprintln!("fatal: cannot add a submodule of a different hash algorithm");
        return Err(GitError::Exit(128));
    }

    write_submodule_mapping(
        &git_dir,
        &worktree_root,
        &normalized_path,
        &options.repository,
        options.branch.as_deref(),
        options.name.as_deref(),
    )?;
    stage_submodule_paths(&git_dir, format, &worktree_root, &normalized_path, head_oid)?;
    Ok(())
}

fn parse_submodule_add_options(args: &[String], mut quiet: bool) -> Result<SubmoduleAddOptions> {
    let mut branch = None;
    let mut name = None;
    let mut force = false;
    let mut values = Vec::new();
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            values.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--force" | "-f" => force = true,
            "--progress" | "--no-progress" => {}
            "--name" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                name = Some(value.clone());
            }
            // Reference repositories are an object-sharing optimization sley
            // does not implement yet; refuse loudly instead of cloning without
            // the requested borrowing.
            "--reference" | "--reference-if-able" => {
                eprintln!("fatal: sley submodule add does not support --reference yet");
                return Err(GitError::Exit(128));
            }
            "--branch" | "-b" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                branch = Some(value.clone());
            }
            value if let Some(value) = value.strip_prefix("--branch=") => {
                branch = Some(value.to_string());
            }
            value if let Some(value) = value.strip_prefix("--name=") => {
                name = Some(value.to_string());
            }
            value
                if value.starts_with("--reference=")
                    || value.starts_with("--reference-if-able=") =>
            {
                eprintln!("fatal: sley submodule add does not support --reference yet");
                return Err(GitError::Exit(128));
            }
            value if value.starts_with('-') => return submodule_usage(),
            value => values.push(value.to_string()),
        }
        index += 1;
    }
    match values.as_slice() {
        [repository] => Ok(SubmoduleAddOptions {
            repository: repository.clone(),
            path: None,
            branch,
            name,
            force,
            quiet,
        }),
        [repository, path] => Ok(SubmoduleAddOptions {
            repository: repository.clone(),
            path: Some(path.clone()),
            branch,
            name,
            force,
            quiet,
        }),
        _ => submodule_usage(),
    }
}

fn cmd_submodule_update(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_update_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    if options.init {
        cmd_submodule_init(
            &options
                .paths
                .iter()
                .map(|path| (*path).to_string())
                .collect::<Vec<_>>(),
            options.quiet,
        )?;
    }
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &options.paths)?;
    let index = read_repository_index(&git_dir, format)?;
    let config = read_repo_config(&git_dir)?;
    for submodule in selected {
        let Some(target_oid) = submodule_index_oid(&index, &submodule.path) else {
            continue;
        };
        let path = worktree_root.join(&submodule.path);
        // `update` (without --init) only touches *initialized* submodules:
        // ones whose url was copied into .git/config. A .gitmodules-only
        // entry gets upstream's two-line stderr nudge and is skipped.
        let Some(url) = config
            .get("submodule", Some(&submodule.name), "url")
            .map(str::to_string)
        else {
            eprintln!("Submodule path '{}' not initialized", submodule.path);
            eprintln!("Maybe you want to use 'update --init'?");
            continue;
        };
        let just_populated = submodule_head(&path).is_err();
        if just_populated {
            // Populate the worktree: reconnect to a retained
            // .git/modules/<path> git dir when one exists (upstream
            // clone_submodule does the same after the worktree was removed),
            // otherwise clone fresh. NOTE: `-N/--no-fetch` only skips the
            // *fetch* step of an update; the native implementation has no
            // separate fetch today, so the flag is accepted as a no-op.
            let modules_git_dir = git_dir.join("modules").join(&submodule.path);
            if modules_git_dir.join("HEAD").is_file() {
                if path.exists() {
                    if !path.is_dir() || fs::read_dir(&path)?.next().is_some() {
                        eprintln!(
                            "fatal: destination path '{}' already exists and is not an empty directory",
                            submodule.path
                        );
                        return Err(GitError::Exit(128));
                    }
                } else {
                    fs::create_dir_all(&path)?;
                }
                rewrite_submodule_gitdir_file(&path, &modules_git_dir)?;
                set_submodule_core_worktree(&path, &modules_git_dir)?;
            } else {
                let mut clone_args = Vec::new();
                if options.quiet {
                    clone_args.push("-q".to_string());
                }
                clone_args.push("--separate-git-dir".to_string());
                clone_args.push(modules_git_dir.display().to_string());
                clone_args.push(url);
                clone_args.push(path.display().to_string());
                super::remote_cmds::cmd_clone(&clone_args)?;
                rewrite_submodule_gitdir_file(&path, &modules_git_dir)?;
                set_submodule_core_worktree(&path, &modules_git_dir)?;
            }
        }
        // Check out the gitlink oid recorded in the superproject index,
        // detached — upstream `submodule update --checkout` runs
        // `git checkout -q <oid>` inside the submodule and reports it. A
        // submodule already at the recorded oid is left alone (and silent) —
        // unless its worktree was just (re)populated: a reconnected git dir
        // can already have HEAD at the target while the worktree is empty.
        let (sub_git_dir, head_oid) = submodule_head(&path)?;
        if just_populated || head_oid != target_oid {
            let sub_format = repository_object_format(&sub_git_dir)?;
            sley_worktree::reset_index_and_worktree_to_commit(
                &path,
                &sub_git_dir,
                sub_format,
                &target_oid,
            )?;
            fs::write(sub_git_dir.join("HEAD"), format!("{target_oid}\n"))?;
            if !options.quiet {
                println!(
                    "Submodule path '{}': checked out '{target_oid}'",
                    submodule.path
                );
            }
        }
    }
    Ok(())
}

fn parse_submodule_update_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleUpdateOptions<'_>> {
    let mut init = false;
    let mut paths = Vec::new();
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            paths.push(arg.as_str());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--init" => init = true,
            // `-N/--no-fetch` skips the fetch step of an update; the native
            // implementation performs no separate fetch, so it is a no-op
            // (NOT "skip checkout" — the checkout below still happens).
            "--no-fetch" | "-N" => {}
            "--checkout"
            | "--merge"
            | "--rebase"
            | "--recursive"
            | "--recommend-shallow"
            | "--no-recommend-shallow"
            | "--single-branch"
            | "--no-single-branch" => {}
            "--filter" => {
                index += 1;
                if args.get(index).is_none() {
                    return submodule_usage();
                }
            }
            // See parse_submodule_add_options: refuse --reference rather than
            // silently cloning without the requested object borrowing.
            "--reference" => {
                eprintln!("fatal: sley submodule update does not support --reference yet");
                return Err(GitError::Exit(128));
            }
            value if value.starts_with("--reference=") => {
                eprintln!("fatal: sley submodule update does not support --reference yet");
                return Err(GitError::Exit(128));
            }
            value if value.starts_with("--filter=") => {}
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
        index += 1;
    }
    Ok(SubmoduleUpdateOptions { init, quiet, paths })
}

fn submodule_usage_text() -> &'static str {
    "usage: git submodule [--quiet] [--cached]\n   or: git submodule [--quiet] add [-b <branch>] [-f|--force] [--name <name>] [--reference <repository>] [--] <repository> [<path>]\n   or: git submodule [--quiet] status [--cached] [--recursive] [--] [<path>...]\n   or: git submodule [--quiet] init [--] [<path>...]\n   or: git submodule [--quiet] deinit [-f|--force] (--all| [--] <path>...)\n   or: git submodule [--quiet] update [--init [--filter=<filter-spec>]] [--remote] [-N|--no-fetch] [-f|--force] [--checkout|--merge|--rebase] [--[no-]recommend-shallow] [--reference <repository>] [--recursive] [--[no-]single-branch] [--] [<path>...]\n   or: git submodule [--quiet] set-branch (--default|--branch <branch>) [--] <path>\n   or: git submodule [--quiet] set-url [--] <path> <newurl>\n   or: git submodule [--quiet] summary [--cached|--files] [--summary-limit <n>] [commit] [--] [<path>...]\n   or: git submodule [--quiet] foreach [--recursive] <command>\n   or: git submodule [--quiet] sync [--recursive] [--] [<path>...]\n   or: git submodule [--quiet] absorbgitdirs [--] [<path>...]"
}

fn submodule_usage<T>() -> Result<T> {
    eprintln!("{}", submodule_usage_text());
    Err(GitError::Exit(1))
}

fn default_submodule_path(repository: &str) -> String {
    let trimmed = repository.trim_end_matches('/');
    let name = Path::new(trimmed)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| trimmed.to_string());
    name.strip_suffix(".git").unwrap_or(&name).to_string()
}

fn normalize_submodule_add_path(cwd: &Path, worktree_root: &Path, path: &str) -> Result<String> {
    let input = Path::new(path.trim_end_matches('/'));
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let normalized = normalize_lexical_path(&absolute);
    let relative = normalized.strip_prefix(worktree_root).map_err(|_| {
        eprintln!("fatal: submodule path '{}' is outside repository", path);
        GitError::Exit(128)
    })?;
    let path = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if path.is_empty() {
        return submodule_usage();
    }
    Ok(path)
}

fn path_matches_or_is_beneath(index_path: &BString, path: &[u8]) -> bool {
    index_path.as_bytes() == path
        || index_path
            .as_bytes()
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with(b"/"))
}

fn rewrite_submodule_gitdir_file(submodule_root: &Path, modules_git_dir: &Path) -> Result<()> {
    let link = relative_path_from_absolute_components(submodule_root, modules_git_dir)?;
    fs::write(submodule_root.join(".git"), format!("gitdir: {link}\n"))?;
    Ok(())
}

fn set_submodule_core_worktree(submodule_root: &Path, modules_git_dir: &Path) -> Result<()> {
    let mut config = read_repo_config(modules_git_dir)?;
    let worktree = relative_path_from_absolute_components(modules_git_dir, submodule_root)?;
    set_config_value(&mut config, "core", None, "worktree", &worktree);
    write_repo_config(modules_git_dir, &config)
}

fn write_submodule_mapping(
    git_dir: &Path,
    worktree_root: &Path,
    path: &str,
    url: &str,
    branch: Option<&str>,
    name_override: Option<&str>,
) -> Result<()> {
    let gitmodules_path = worktree_root.join(".gitmodules");
    let mut gitmodules = GitConfig::read(&gitmodules_path).unwrap_or_default();
    let name = name_override.map(str::to_string).unwrap_or_else(|| {
        submodule_name_for_exact_path(&gitmodules, path).unwrap_or_else(|| path.to_string())
    });
    set_submodule_config_value(&mut gitmodules, &name, "path", path);
    set_submodule_config_value(&mut gitmodules, &name, "url", url);
    if let Some(branch) = branch {
        set_submodule_config_value(&mut gitmodules, &name, "branch", branch);
    }
    fs::write(&gitmodules_path, gitmodules.to_canonical_bytes())?;

    let mut config = read_repo_config(git_dir)?;
    set_submodule_config_value(&mut config, &name, "url", url);
    set_submodule_config_value(&mut config, &name, "active", "true");
    write_repo_config(git_dir, &config)?;
    Ok(())
}

fn stage_submodule_paths(
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    path: &str,
    oid: ObjectId,
) -> Result<()> {
    super::plumbing::cmd_add(&[worktree_root.join(".gitmodules").display().to_string()])?;
    sley_worktree::update_index_cacheinfo(
        git_dir,
        format,
        &[sley_worktree::CacheInfoEntry {
            mode: 0o160000,
            oid,
            path: path.as_bytes().to_vec(),
            stage: 0,
        }],
        true,
        false,
    )?;
    Ok(())
}

fn cmd_submodule_init(args: &[String], quiet: bool) -> Result<()> {
    let (paths, quiet) = parse_submodule_init_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &paths)?;
    let mut config = read_repo_config(&git_dir)?;
    let mut changed = false;
    for submodule in selected {
        if config
            .get("submodule", Some(&submodule.name), "url")
            .is_some()
        {
            continue;
        }
        let Some(url) = &submodule.url else {
            continue;
        };
        let url = resolve_submodule_init_url(&worktree_root, &config, url);
        set_submodule_config_value(&mut config, &submodule.name, "active", "true");
        set_submodule_config_value(&mut config, &submodule.name, "url", &url);
        if let Some(update) = &submodule.update {
            set_submodule_config_value(&mut config, &submodule.name, "update", update);
        }
        if !quiet {
            eprintln!(
                "Submodule '{}' ({}) registered for path '{}'",
                submodule.name, url, submodule.path
            );
        }
        changed = true;
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

fn cmd_submodule_deinit(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_deinit_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = if options.all {
        submodules.iter().collect::<Vec<_>>()
    } else {
        if options.paths.is_empty() {
            eprintln!("fatal: Use '--all' if you really want to deinitialize all submodules");
            return Err(GitError::Exit(128));
        }
        filter_submodule_configs(&cwd, &worktree_root, &submodules, &options.paths)?
    };
    let mut config = read_repo_config(&git_dir)?;
    let mut changed = false;
    for submodule in selected {
        let Some(url) = config
            .get("submodule", Some(&submodule.name), "url")
            .map(str::to_string)
            .or_else(|| submodule.url.clone())
        else {
            continue;
        };
        if !options.force && submodule_worktree_has_local_changes(&worktree_root, submodule)? {
            eprintln!("error: the following file has local modifications:");
            eprintln!("    {}", submodule.path);
            eprintln!("(use --cached to keep the file, or -f to force removal)");
            eprintln!(
                "fatal: Submodule work tree '{}' contains local modifications; use '-f' to discard them",
                submodule.path
            );
            return Err(GitError::Exit(128));
        }
        clear_submodule_worktree(&worktree_root.join(&submodule.path))?;
        remove_submodule_config_section(&mut config, &submodule.name);
        if !options.quiet {
            println!("Cleared directory '{}'", submodule.path);
            println!(
                "Submodule '{}' ({}) unregistered for path '{}'",
                submodule.name, url, submodule.path
            );
        }
        changed = true;
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

fn cmd_submodule_sync(args: &[String], quiet: bool) -> Result<()> {
    let (paths, quiet, _recursive) = parse_submodule_sync_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &paths)?;
    let mut config = read_repo_config(&git_dir)?;
    let mut changed = false;
    for submodule in selected {
        if config
            .get("submodule", Some(&submodule.name), "url")
            .is_none()
        {
            continue;
        }
        let Some(url) = &submodule.url else {
            continue;
        };
        let url = resolve_submodule_sync_url(&worktree_root, &config, url);
        set_submodule_config_value(&mut config, &submodule.name, "url", &url);
        if !quiet {
            println!("Synchronizing submodule url for '{}'", submodule.path);
        }
        changed = true;
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

fn cmd_submodule_absorbgitdirs(args: &[String], quiet: bool) -> Result<()> {
    let (paths, quiet) = parse_submodule_absorbgitdirs_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &paths)?;
    for submodule in selected {
        absorb_submodule_git_dir(&git_dir, &worktree_root, submodule, quiet)?;
    }
    Ok(())
}

fn cmd_submodule_foreach(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_foreach_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let index = read_repository_index(&git_dir, format)?;
    run_submodule_foreach_tree(&cwd, &worktree_root, &index, &submodules, &options)
}

fn cmd_submodule_summary(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_summary_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    if options.quiet || options.summary_limit == Some(0) {
        return Ok(());
    }
    let submodules = read_submodule_configs(&worktree_root)?;
    let index = read_repository_index(&git_dir, format)?;
    let selected = select_submodules_for_summary(&cwd, &worktree_root, &submodules, &options);
    for submodule in selected {
        print_submodule_summary(
            &cwd,
            &git_dir,
            &worktree_root,
            &index,
            submodule,
            options.cached,
            options.summary_limit,
        )?;
    }
    Ok(())
}

fn cmd_submodule_set_url(args: &[String], quiet: bool) -> Result<()> {
    let (path, new_url, quiet) = parse_submodule_set_url_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let gitmodules_path = worktree_root.join(".gitmodules");
    let mut gitmodules = GitConfig::read(&gitmodules_path)?;
    let Some(name) = submodule_name_for_exact_path(&gitmodules, path) else {
        eprintln!("fatal: no submodule mapping found in .gitmodules for path '{path}'");
        return Err(GitError::Exit(128));
    };
    set_submodule_config_value(&mut gitmodules, &name, "url", new_url);
    fs::write(&gitmodules_path, gitmodules.to_canonical_bytes())?;

    let mut config = read_repo_config(&git_dir)?;
    if config.get("submodule", Some(&name), "url").is_some() {
        set_submodule_config_value(&mut config, &name, "url", new_url);
        write_repo_config(&git_dir, &config)?;
        if !quiet {
            println!("Synchronizing submodule url for '{path}'");
        }
    }
    Ok(())
}

enum SubmoduleSetBranchAction<'a> {
    Branch(&'a str),
    Default,
}

fn cmd_submodule_set_branch(args: &[String], quiet: bool) -> Result<()> {
    let (path, action, _quiet) = parse_submodule_set_branch_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let gitmodules_path = worktree_root.join(".gitmodules");
    let mut gitmodules = GitConfig::read(&gitmodules_path)?;
    let Some(name) = submodule_name_for_exact_path(&gitmodules, path) else {
        eprintln!("fatal: no submodule mapping found in .gitmodules for path '{path}'");
        return Err(GitError::Exit(128));
    };
    match action {
        SubmoduleSetBranchAction::Branch(branch) => {
            set_submodule_config_value(&mut gitmodules, &name, "branch", branch);
        }
        SubmoduleSetBranchAction::Default => {
            unset_submodule_config_value(&mut gitmodules, &name, "branch");
        }
    }
    fs::write(&gitmodules_path, gitmodules.to_canonical_bytes())?;
    Ok(())
}

fn parse_submodule_init_options(args: &[String], mut quiet: bool) -> Result<(Vec<&str>, bool)> {
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet))
}

struct SubmoduleDeinitOptions<'a> {
    all: bool,
    force: bool,
    quiet: bool,
    paths: Vec<&'a str>,
}

struct SubmoduleForeachOptions {
    command: String,
    quiet: bool,
    recursive: bool,
}

struct SubmoduleSummaryOptions {
    cached: bool,
    quiet: bool,
    summary_limit: Option<isize>,
    positionals: Vec<String>,
}

fn parse_submodule_deinit_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleDeinitOptions<'_>> {
    let mut all = false;
    let mut force = false;
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--all" => all = true,
            "--quiet" | "-q" => quiet = true,
            "-f" | "--force" => force = true,
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok(SubmoduleDeinitOptions {
        all,
        force,
        quiet,
        paths,
    })
}

fn parse_submodule_set_branch_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(&str, SubmoduleSetBranchAction<'_>, bool)> {
    let mut branch = None;
    let mut default = false;
    let mut values = Vec::new();
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            values.push(arg.as_str());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--default" | "-d" => default = true,
            "--branch" | "-b" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                branch = Some(value.as_str());
            }
            value if let Some(value) = value.strip_prefix("--branch=") => {
                branch = Some(value);
            }
            "--no-default" | "--no-branch" => return submodule_usage(),
            value if value.starts_with('-') => return submodule_usage(),
            value => values.push(value),
        }
        index += 1;
    }
    if branch.is_none() && !default {
        eprintln!("fatal: --branch or --default required");
        return Err(GitError::Exit(128));
    }
    if branch.is_some() && default {
        eprintln!("fatal: options '--branch' and '--default' cannot be used together");
        return Err(GitError::Exit(128));
    }
    match (values.as_slice(), branch, default) {
        ([path], Some(branch), false) => {
            Ok((path, SubmoduleSetBranchAction::Branch(branch), quiet))
        }
        ([path], None, true) => Ok((path, SubmoduleSetBranchAction::Default, quiet)),
        _ => submodule_set_branch_usage(),
    }
}

fn submodule_set_branch_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git submodule set-branch [-q|--quiet] (-d|--default) <path>\n   or: git submodule set-branch [-q|--quiet] (-b|--branch) <branch> <path>\n\n    -d, --[no-]default    set the default tracking branch to master\n    -b, --[no-]branch <branch>\n                          set the default tracking branch\n"
    );
    Err(GitError::Exit(129))
}

fn parse_submodule_set_url_options(args: &[String], mut quiet: bool) -> Result<(&str, &str, bool)> {
    let mut values = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            values.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--no-quiet" => quiet = false,
            value if value.starts_with('-') => return submodule_set_url_usage(),
            value => values.push(value),
        }
    }
    match values.as_slice() {
        [path, new_url] => Ok((path, new_url, quiet)),
        _ => submodule_set_url_usage(),
    }
}

fn submodule_set_url_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git submodule set-url [--quiet] <path> <newurl>\n\n    -q, --[no-]quiet      suppress output for setting url of a submodule\n"
    );
    Err(GitError::Exit(129))
}

fn parse_submodule_sync_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool, bool)> {
    let mut paths = Vec::new();
    let mut positional_only = false;
    let mut recursive = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--recursive" => recursive = true,
            "--no-recursive" => return submodule_usage(),
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet, recursive))
}

fn parse_submodule_absorbgitdirs_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool)> {
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet))
}

fn parse_submodule_foreach_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleForeachOptions> {
    let mut recursive = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--quiet" | "-q" => {
                quiet = true;
                index += 1;
            }
            "--recursive" => {
                recursive = true;
                index += 1;
            }
            "--" => {
                index += 1;
                break;
            }
            value if value.starts_with('-') => return submodule_usage(),
            _ => break,
        }
    }
    Ok(SubmoduleForeachOptions {
        command: args[index..].join(" "),
        quiet,
        recursive,
    })
}

fn parse_submodule_summary_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleSummaryOptions> {
    let mut cached = false;
    let mut files = false;
    let mut summary_limit = None;
    let mut positionals = Vec::new();
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            positionals.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--cached" => cached = true,
            "--files" => files = true,
            "--summary-limit" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                summary_limit = Some(parse_submodule_summary_limit(value)?);
            }
            value if let Some(value) = value.strip_prefix("--summary-limit=") => {
                summary_limit = Some(parse_submodule_summary_limit(value)?);
            }
            value if value.starts_with('-') => return submodule_usage(),
            value => positionals.push(value.to_string()),
        }
        index += 1;
    }
    if cached && files {
        eprintln!("fatal: options '--cached' and '--files' cannot be used together");
        return Err(GitError::Exit(128));
    }
    Ok(SubmoduleSummaryOptions {
        cached,
        quiet,
        summary_limit,
        positionals,
    })
}

fn parse_submodule_summary_limit(value: &str) -> Result<isize> {
    value.parse::<isize>().map_err(|_| {
        eprintln!(
            "error: option `summary-limit' expects an integer value with an optional k/m/g suffix"
        );
        GitError::Exit(129)
    })
}

fn submodule_name_for_exact_path(config: &GitConfig, path: &str) -> Option<String> {
    config
        .sections
        .iter()
        .filter(|section| section.name == "submodule")
        .find(|section| {
            section
                .entries
                .iter()
                .rev()
                .find(|entry| entry.key == "path")
                .and_then(|entry| entry.value.as_deref())
                == Some(path)
        })
        .and_then(|section| section.subsection.clone())
}

fn filter_submodule_configs<'a>(
    cwd: &Path,
    worktree_root: &Path,
    submodules: &'a [SubmoduleConfigEntry],
    paths: &[&str],
) -> Result<Vec<&'a SubmoduleConfigEntry>> {
    if paths.is_empty() {
        return Ok(submodules.iter().collect());
    }
    let mut selected = Vec::new();
    for path in paths {
        let normalized = normalize_submodule_pathspec(cwd, worktree_root, path);
        let matching = submodules
            .iter()
            .filter(|submodule| submodule_path_matches_pathspec(&submodule.path, &normalized))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            eprintln!("error: pathspec '{path}' did not match any file(s) known to git");
            return Err(GitError::Exit(1));
        }
        selected.extend(matching);
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    selected.dedup_by(|left, right| left.path == right.path);
    Ok(selected)
}

fn select_submodules_for_summary<'a>(
    cwd: &Path,
    worktree_root: &Path,
    submodules: &'a [SubmoduleConfigEntry],
    options: &SubmoduleSummaryOptions,
) -> Vec<&'a SubmoduleConfigEntry> {
    if options.positionals.is_empty() {
        return submodules.iter().collect();
    }
    let mut selected = Vec::new();
    for path in &options.positionals {
        let normalized = normalize_submodule_pathspec(cwd, worktree_root, path);
        selected.extend(
            submodules
                .iter()
                .filter(|submodule| submodule_path_matches_pathspec(&submodule.path, &normalized)),
        );
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    selected.dedup_by(|left, right| left.path == right.path);
    selected
}

fn resolve_submodule_init_url(worktree_root: &Path, config: &GitConfig, url: &str) -> String {
    if !(url.starts_with("../") || url.starts_with("./")) {
        return url.to_string();
    }
    let base = config
        .get("remote", Some("origin"), "url")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .and_then(|path| path.parent().map(|parent| parent.join(url)))
        .unwrap_or_else(|| {
            eprintln!(
                "warning: could not look up configuration 'remote.origin.url'. Assuming this repository is its own authoritative upstream."
            );
            worktree_root.join(url)
        });
    normalize_lexical_path(&base).display().to_string()
}

fn resolve_submodule_sync_url(worktree_root: &Path, config: &GitConfig, url: &str) -> String {
    if !(url.starts_with("../") || url.starts_with("./")) {
        return url.to_string();
    }
    let base = config
        .get("remote", Some("origin"), "url")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .and_then(|path| path.parent().map(|parent| parent.join(url)))
        .unwrap_or_else(|| worktree_root.join(url));
    normalize_lexical_path(&base).display().to_string()
}

fn set_submodule_config_value(config: &mut GitConfig, name: &str, key: &str, value: &str) {
    set_config_value(config, "submodule", Some(name), key, value);
}

fn unset_submodule_config_value(config: &mut GitConfig, name: &str, key: &str) {
    let Some(section) =
        config.sections.iter_mut().rev().find(|section| {
            section.name == "submodule" && section.subsection.as_deref() == Some(name)
        })
    else {
        return;
    };
    section
        .entries
        .retain(|entry| !entry.key.eq_ignore_ascii_case(key));
}

fn remove_submodule_config_section(config: &mut GitConfig, name: &str) {
    config.sections.retain(|section| {
        !(section.name == "submodule" && section.subsection.as_deref() == Some(name))
    });
}

fn submodule_worktree_has_local_changes(
    worktree_root: &Path,
    submodule: &SubmoduleConfigEntry,
) -> Result<bool> {
    let submodule_root = worktree_root.join(&submodule.path);
    if !submodule_root.exists() {
        return Ok(false);
    }
    let Ok((git_dir, _)) = submodule_head(&submodule_root) else {
        return Ok(false);
    };
    let format = repository_object_format(&git_dir)?;
    let Some(index) = read_repository_index(&git_dir, format)? else {
        return submodule_worktree_has_entries(&submodule_root);
    };
    let mut tracked = BTreeSet::new();
    for entry in &index.entries {
        tracked.insert(String::from_utf8_lossy(&entry.path).into_owned());
        let path = submodule_root.join(String::from_utf8_lossy(&entry.path).as_ref());
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
    submodule_worktree_has_untracked_entries(&submodule_root, &submodule_root, &tracked)
}

fn absorb_submodule_git_dir(
    git_dir: &Path,
    worktree_root: &Path,
    submodule: &SubmoduleConfigEntry,
    quiet: bool,
) -> Result<()> {
    let submodule_root = worktree_root.join(&submodule.path);
    let dot_git = submodule_root.join(".git");
    if !dot_git.is_dir() {
        return Ok(());
    }
    let modules_git_dir = git_dir.join("modules").join(&submodule.path);
    let Some(parent) = modules_git_dir.parent() else {
        return Err(GitError::InvalidPath(format!(
            "invalid submodule gitdir path {}",
            modules_git_dir.display()
        )));
    };
    fs::create_dir_all(parent)?;
    let from_display = fs::canonicalize(&dot_git)?;
    let to_display = if modules_git_dir.exists() {
        fs::canonicalize(&modules_git_dir)?
    } else {
        fs::canonicalize(parent)?.join(
            modules_git_dir
                .file_name()
                .ok_or_else(|| GitError::InvalidPath("invalid submodule gitdir".into()))?,
        )
    };
    if !quiet {
        eprintln!("Migrating git directory of '{}' from", submodule.path);
        eprintln!("'{}' to", from_display.display());
        eprintln!("'{}'", to_display.display());
    }
    fs::rename(&dot_git, &modules_git_dir)?;

    let gitdir_link = relative_path_from_absolute_components(&submodule_root, &modules_git_dir)?;
    fs::write(&dot_git, format!("gitdir: {gitdir_link}\n"))?;

    let mut config = read_repo_config(&modules_git_dir)?;
    let worktree = relative_path_from_absolute_components(&modules_git_dir, &submodule_root)?;
    set_config_value(&mut config, "core", None, "worktree", &worktree);
    write_repo_config(&modules_git_dir, &config)?;
    Ok(())
}

fn run_submodule_foreach_tree(
    cwd: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodules: &[SubmoduleConfigEntry],
    options: &SubmoduleForeachOptions,
) -> Result<()> {
    let selected = filter_submodule_configs(cwd, worktree_root, submodules, &[])?;
    for submodule in selected {
        let submodule_root = worktree_root.join(&submodule.path);
        let Ok((submodule_git_dir, _)) = submodule_head(&submodule_root) else {
            continue;
        };
        run_submodule_foreach_command(cwd, worktree_root, index, submodule, options)?;
        if options.recursive {
            let nested_configs = read_submodule_configs(&submodule_root)?;
            let nested_format = repository_object_format(&submodule_git_dir)?;
            let nested_index = read_repository_index(&submodule_git_dir, nested_format)?;
            run_submodule_foreach_tree(
                cwd,
                &submodule_root,
                &nested_index,
                &nested_configs,
                options,
            )?;
        }
    }
    Ok(())
}

fn run_submodule_foreach_command(
    cwd: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodule: &SubmoduleConfigEntry,
    options: &SubmoduleForeachOptions,
) -> Result<()> {
    let submodule_root = worktree_root.join(&submodule.path);
    let display_path = display_submodule_path(cwd, worktree_root, &submodule.path)?;
    let sha1 = submodule_index_oid(index, &submodule.path)
        .map(|oid| oid.to_string())
        .unwrap_or_default();
    if !options.quiet {
        println!("Entering '{display_path}'");
    }
    let output = ProcessCommand::new("sh")
        .arg("-c")
        .arg(&options.command)
        .current_dir(&submodule_root)
        .env("name", &submodule.name)
        .env("sm_path", &submodule.path)
        .env("displaypath", &display_path)
        .env("sha1", &sha1)
        .env("toplevel", worktree_root)
        .output()
        .map_err(|err| GitError::Io(err.to_string()))?;
    io::stdout().write_all(&output.stdout)?;
    io::stderr().write_all(&output.stderr)?;
    if output.status.success() {
        return Ok(());
    }
    eprintln!("fatal: run_command returned non-zero status for {display_path}");
    eprintln!(".");
    Err(GitError::Exit(128))
}

fn print_submodule_summary(
    cwd: &Path,
    git_dir: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodule: &SubmoduleConfigEntry,
    cached: bool,
    summary_limit: Option<isize>,
) -> Result<()> {
    let Some(index_oid) = submodule_index_oid(index, &submodule.path) else {
        return Ok(());
    };
    let submodule_root = worktree_root.join(&submodule.path);
    let Ok((submodule_git_dir, head_oid)) = submodule_head(&submodule_root) else {
        return Ok(());
    };
    let old_oid = if cached {
        let format = repository_object_format(git_dir)?;
        let Some(head_index_oid) = submodule_head_tree_oid(git_dir, format, &submodule.path)?
        else {
            return Ok(());
        };
        head_index_oid
    } else {
        index_oid
    };
    let new_oid = if cached { index_oid } else { head_oid };
    if new_oid == old_oid {
        return Ok(());
    }
    let format = repository_object_format(&submodule_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&submodule_git_dir, format);
    let (marker, commits) = submodule_summary_commits(&db, format, &old_oid, &new_oid)?;
    if commits.is_empty() {
        return Ok(());
    }
    let display_path = display_submodule_path(cwd, worktree_root, &submodule.path)?;
    println!(
        "* {} {}...{} ({}):",
        display_path,
        format_log_abbrev_oid(&old_oid),
        format_log_abbrev_oid(&new_oid),
        commits.len()
    );
    let limit = summary_limit
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(commits.len());
    for (_, commit) in commits.iter().take(limit) {
        println!("  {marker} {}", commit_subject(&commit.message));
    }
    println!();
    Ok(())
}

fn submodule_head_tree_oid(
    git_dir: &Path,
    format: ObjectFormat,
    path: &str,
) -> Result<Option<ObjectId>> {
    let Ok(head_oid) = resolve_revision(git_dir, format, "HEAD") else {
        return Ok(None);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&head_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            head_oid,
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let tree_object = db.read_object(&commit.tree)?;
    if tree_object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {}, found {}",
            commit.tree,
            tree_object.object_type.as_str()
        )));
    }
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let Some(entry) = find_tree_entry(&db, format, &tree_object.body, &components)? else {
        return Ok(None);
    };
    if entry.mode != 0o160000 {
        return Ok(None);
    }
    Ok(Some(entry.oid))
}

fn submodule_summary_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    index_oid: &ObjectId,
    head_oid: &ObjectId,
) -> Result<(char, Vec<(ObjectId, Commit)>)> {
    let forward = submodule_summary_forward_commits(db, format, index_oid, head_oid)?;
    if !forward.is_empty() {
        return Ok(('>', forward));
    }
    let reverse = submodule_summary_forward_commits(db, format, head_oid, index_oid)?;
    Ok(('<', reverse))
}

fn submodule_summary_forward_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
) -> Result<Vec<(ObjectId, Commit)>> {
    let old_ancestors = ancestor_depths(db, format, old_oid)?;
    let mut commits = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from([*new_oid]);
    while let Some(oid) = pending.pop_front() {
        if old_ancestors.contains_key(&oid) || !seen.insert(oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            continue;
        }
        let commit = Commit::parse(format, &object.body)?;
        pending.extend(commit.parents.iter().copied());
        commits.push((oid, commit));
    }
    Ok(commits)
}

fn submodule_index_oid(index: &Option<Index>, path: &str) -> Option<ObjectId> {
    let path = path.as_bytes();
    index
        .as_ref()?
        .entries
        .iter()
        .find(|entry| entry.mode == 0o160000 && entry.path == path)
        .map(|entry| entry.oid)
}

fn submodule_worktree_has_entries(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

fn clear_submodule_worktree(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn read_submodule_configs(worktree_root: &Path) -> Result<Vec<SubmoduleConfigEntry>> {
    let path = worktree_root.join(".gitmodules");
    let Ok(config) = GitConfig::read(path) else {
        return Ok(Vec::new());
    };
    let mut submodules = Vec::new();
    for section in config.sections {
        if section.name != "submodule" {
            continue;
        }
        let Some(name) = section.subsection.clone() else {
            continue;
        };
        let path = section
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key == "path")
            .and_then(|entry| entry.value.clone());
        let url = section
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key == "url")
            .and_then(|entry| entry.value.clone());
        let update = section
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key == "update")
            .and_then(|entry| entry.value.clone());
        if let Some(path) = path {
            submodules.push(SubmoduleConfigEntry {
                name,
                path,
                url,
                update,
            });
        }
    }
    Ok(submodules)
}

fn filter_submodules(
    cwd: &Path,
    worktree_root: &Path,
    submodules: Vec<SubmoduleConfigEntry>,
    paths: &[&str],
) -> Result<Vec<SubmoduleStatusEntry>> {
    if paths.is_empty() {
        let mut selected = submodules
            .into_iter()
            .map(|submodule| {
                let display_path = display_submodule_path(cwd, worktree_root, &submodule.path)?;
                Ok(SubmoduleStatusEntry {
                    path: submodule.path,
                    display_path,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        selected.sort_by(|left, right| left.path.cmp(&right.path));
        return Ok(selected);
    }
    let mut selected = Vec::new();
    for path in paths {
        let normalized = normalize_submodule_pathspec(cwd, worktree_root, path);
        let matching = submodules
            .iter()
            .filter(|submodule| submodule_path_matches_pathspec(&submodule.path, &normalized))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            eprintln!("error: pathspec '{path}' did not match any file(s) known to git");
            return Err(GitError::Exit(1));
        }
        for submodule in matching {
            selected.push(SubmoduleStatusEntry {
                path: submodule.path.clone(),
                display_path: display_submodule_path(cwd, worktree_root, &submodule.path)?,
            });
        }
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    selected.dedup_by(|left, right| left.path == right.path);
    Ok(selected)
}

fn submodule_path_matches_pathspec(path: &str, pathspec: &str) -> bool {
    pathspec.is_empty()
        || path == pathspec
        || path
            .strip_prefix(pathspec)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn normalize_submodule_pathspec(cwd: &Path, worktree_root: &Path, path: &str) -> String {
    let path = path.trim_end_matches('/');
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let root = fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    lexical_relative_path(&root, &absolute).unwrap_or_else(|| {
        path.to_string_lossy()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string()
    })
}

fn display_submodule_path(cwd: &Path, worktree_root: &Path, path: &str) -> Result<String> {
    let absolute = fs::canonicalize(worktree_root)?.join(path);
    relative_path_from_absolute(cwd, &absolute).map(|path| path.trim_end_matches('/').to_string())
}

fn lexical_relative_path(root: &Path, target: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in target.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::Normal(value) => parts.push(value.to_os_string()),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                parts.clear();
                parts.push(component.as_os_str().to_os_string());
            }
        }
    }
    let normalized = parts.into_iter().collect::<PathBuf>();
    let relative = normalized.strip_prefix(root).ok()?;
    Some(
        relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn print_submodule_status_tree(
    cwd: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodule: &SubmoduleStatusEntry,
    cached: bool,
    recursive: bool,
) -> Result<()> {
    print_submodule_status(worktree_root, index, submodule, cached)?;
    if !recursive {
        return Ok(());
    }
    let submodule_root = worktree_root.join(&submodule.path);
    let Ok((git_dir, _)) = submodule_head(&submodule_root) else {
        return Ok(());
    };
    let nested_configs = read_submodule_configs(&submodule_root)?;
    let nested = filter_submodules(cwd, &submodule_root, nested_configs, &[])?;
    let nested_format = repository_object_format(&git_dir)?;
    let nested_index = read_repository_index(&git_dir, nested_format)?;
    for nested_submodule in nested {
        print_submodule_status_tree(
            cwd,
            &submodule_root,
            &nested_index,
            &nested_submodule,
            cached,
            recursive,
        )?;
    }
    Ok(())
}

fn print_submodule_status(
    worktree_root: &Path,
    index: &Option<Index>,
    submodule: &SubmoduleStatusEntry,
    cached: bool,
) -> Result<()> {
    let path_bytes = submodule.path.as_bytes();
    let cached_oid = index
        .as_ref()
        .and_then(|index| {
            index
                .entries
                .iter()
                .find(|entry| entry.mode == 0o160000 && entry.path == path_bytes)
        })
        .map(|entry| entry.oid);
    let Some(cached_oid) = cached_oid else {
        return Ok(());
    };

    let submodule_root = worktree_root.join(&submodule.path);
    let submodule_head = submodule_head(&submodule_root).ok();
    let prefix = if submodule_head.is_none() {
        '-'
    } else if submodule_head
        .as_ref()
        .is_some_and(|(_, oid)| oid != &cached_oid)
    {
        '+'
    } else {
        ' '
    };
    let output_oid = if cached {
        cached_oid
    } else {
        submodule_head
            .as_ref()
            .map(|(_, oid)| *oid)
            .unwrap_or(cached_oid)
    };
    let suffix = submodule_status_suffix(
        submodule_head
            .as_ref()
            .map(|(git_dir, _)| git_dir.as_path()),
        &output_oid,
    )?;
    println!("{prefix}{output_oid} {}{suffix}", submodule.display_path);
    Ok(())
}

fn submodule_head(submodule_root: &Path) -> Result<(PathBuf, ObjectId)> {
    let dot_git = submodule_root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let Some(git_dir) = read_gitdir_file(&dot_git)? else {
            return Err(GitError::not_found("submodule gitdir"));
        };
        git_dir
    } else {
        return Err(GitError::not_found("submodule gitdir"));
    };
    let format = repository_object_format(&git_dir)?;
    let oid = sley_rev::resolve_revision(&git_dir, format, "HEAD")?;
    Ok((git_dir, oid))
}

fn submodule_status_suffix(git_dir: Option<&Path>, oid: &ObjectId) -> Result<String> {
    let Some(git_dir) = git_dir else {
        return Ok(String::new());
    };
    let format = repository_object_format(git_dir)?;
    let store = FileRefStore::new(git_dir, format);
    let refs = store.list_refs()?;
    for reference in refs
        .iter()
        .filter(|reference| reference.name.starts_with("refs/tags/"))
    {
        if let Some((target_oid, _)) = resolve_for_each_ref_target(&store, reference)?
            && target_oid == *oid
        {
            return Ok(format!(" ({})", display_submodule_ref(&reference.name)));
        }
    }
    if let Some(RefTarget::Symbolic(target)) = store.read_ref("HEAD")?
        && let Some(target_oid) = resolve_ref_to_oid(&store, &target)?
        && target_oid == *oid
    {
        return Ok(format!(" ({})", display_submodule_ref(&target)));
    }
    for reference in refs {
        if reference.name.starts_with("refs/tags/") {
            continue;
        }
        if let Some((target_oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && target_oid == *oid
        {
            return Ok(format!(" ({})", display_submodule_ref(&reference.name)));
        }
    }
    Ok(String::new())
}

fn display_submodule_ref(name: &str) -> String {
    if let Some(tag) = name.strip_prefix("refs/tags/") {
        return tag.to_string();
    }
    name.strip_prefix("refs/").unwrap_or(name).to_string()
}
