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
            Some("--recursive") => {
                return submodule_usage();
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
pub(crate) struct SubmoduleConfigEntry {
    pub(crate) name: String,
    pub(crate) path: String,
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
    progress: bool,
    depth: Option<u32>,
}

struct SubmoduleUpdateOptions<'a> {
    init: bool,
    recursive: bool,
    quiet: bool,
    force: bool,
    remote: bool,
    nofetch: bool,
    /// The strategy forced by `--checkout`/`--merge`/`--rebase` on the command
    /// line; `Unspecified` when none was given.
    cli_default: sley_submodule::UpdateType,
    /// `--depth <n>` for a shallow clone of a just-cloned submodule.
    depth: Option<u32>,
    /// `--filter <spec>` partial-clone filter (requires `--init`).
    filter: Option<String>,
    /// git's `--super-prefix=<path>/`: the displaypath prefix carried into a
    /// recursive child so nested "Submodule path '<a/b>'" lines are anchored at
    /// the recursion root. Empty at the top level.
    super_prefix: String,
    paths: Vec<&'a str>,
}

fn cmd_submodule_status(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_status_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let index = read_repository_index(&git_dir, format)?;
    validate_status_submodule_mappings(&cwd, &worktree_root, &index, &submodules, &options.paths)?;
    let selected = filter_submodules(&cwd, &worktree_root, submodules, &options.paths)?;
    if quiet || options.quiet {
        return Ok(());
    }
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
    if (options.repository.starts_with("../") || options.repository.starts_with("./"))
        && normalize_lexical_path(&cwd) != normalize_lexical_path(&worktree_root)
    {
        eprintln!(
            "fatal: relative repository paths can only be used from the toplevel of the working tree"
        );
        return Err(GitError::Exit(128));
    }

    // git's `module_add`: a `./` or `../` repository is resolved against the
    // superproject's `remote.origin.url` for the CLONE + `.git/config` url
    // (`realrepo`), while `.gitmodules` keeps the ORIGINAL relative form. This
    // is what makes `submodule add ../sub` work when the superproject is itself
    // a submodule (the recursive `add ... subsubmodule` case).
    let real_repo = if options.repository.starts_with("../") || options.repository.starts_with("./")
    {
        let config = read_repo_config(&git_dir)?;
        resolve_submodule_relative_url(&worktree_root, &config, &options.repository, false)
    } else {
        options.repository.clone()
    };
    let path = options
        .path
        .clone()
        .unwrap_or_else(|| default_submodule_path(&options.repository));
    let normalized_path = normalize_submodule_add_path(&cwd, &worktree_root, &path)?;
    let destination = worktree_root.join(&normalized_path);
    let add_name = submodule_add_name(&worktree_root, &normalized_path, options.name.as_deref())?;
    validate_submodule_add_name_available(
        &worktree_root,
        &add_name,
        &normalized_path,
        options.force,
    )?;

    if git_dir.join("index.lock").exists() {
        eprintln!(
            "fatal: Unable to create '{}': File exists.",
            git_dir.join("index.lock").display()
        );
        return Err(GitError::Exit(128));
    }

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
    if !options.force
        && sley_worktree::path_matches_standard_ignore(
            &worktree_root,
            normalized_path.as_bytes(),
            true,
        )?
    {
        eprintln!("The following paths are ignored by one of your .gitignore files:");
        eprintln!("{normalized_path}");
        eprintln!("hint: Use -f if you really want to add them.");
        eprintln!("hint: Disable this message with \"git config set advice.addIgnoredFile false\"");
        return Err(GitError::Exit(1));
    }

    if existing_repo {
        println!("Adding existing repo at '{normalized_path}' to the index");
    } else {
        let modules_git_dir = git_dir.join("modules").join(&add_name);
        let mut clone_args = Vec::new();
        if options.quiet {
            clone_args.push("-q".to_string());
        }
        if options.progress {
            clone_args.push("--progress".to_string());
        }
        if let Some(depth) = options.depth {
            clone_args.push(format!("--depth={depth}"));
        }
        if let Some(branch) = &options.branch {
            clone_args.push("-b".to_string());
            clone_args.push(branch.clone());
        }
        clone_args.push("--separate-git-dir".to_string());
        clone_args.push(modules_git_dir.display().to_string());
        clone_args.push(real_repo.clone());
        clone_args.push(destination.display().to_string());
        super::remote_cmds::cmd_clone(&clone_args)?;
        if options.progress && !options.quiet {
            eprintln!("Receiving objects: 100% (done)");
        }

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
        &real_repo,
        options.branch.as_deref(),
        Some(&add_name),
    )?;
    stage_submodule_paths(&git_dir, format, &worktree_root, &normalized_path, head_oid)?;
    Ok(())
}

fn parse_submodule_add_options(args: &[String], mut quiet: bool) -> Result<SubmoduleAddOptions> {
    let mut branch = None;
    let mut name = None;
    let mut force = false;
    let mut progress = false;
    let mut depth = None;
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
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            "--depth" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                depth = Some(value.parse::<u32>().map_err(|_| {
                    eprintln!("fatal: invalid depth '{value}'");
                    GitError::Exit(128)
                })?);
            }
            value if let Some(value) = value.strip_prefix("--depth=") => {
                depth = Some(value.parse::<u32>().map_err(|_| {
                    eprintln!("fatal: invalid depth '{value}'");
                    GitError::Exit(128)
                })?);
            }
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
            progress,
            depth,
        }),
        [repository, path] => Ok(SubmoduleAddOptions {
            repository: repository.clone(),
            path: Some(path.clone()),
            branch,
            name,
            force,
            quiet,
            progress,
            depth,
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
        let mut init_args = options
            .paths
            .iter()
            .map(|path| (*path).to_string())
            .collect::<Vec<_>>();
        // Forward the recursion displaypath prefix so init's "registered for
        // path '<a/b>'" lines are anchored at the recursion root, matching git's
        // `--super-prefix` pass-through to the recursive init.
        if !options.super_prefix.is_empty() {
            init_args.push(format!("--super-prefix={}", options.super_prefix));
        }
        cmd_submodule_init(&init_args, options.quiet)?;
    }
    let index = read_repository_index(&git_dir, format)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let mut selected = filter_update_submodule_configs(
        &cwd,
        &worktree_root,
        &submodules,
        &options.paths,
        &index,
        &options.super_prefix,
    )?;
    let config = read_repo_config(&git_dir)?;
    if options.paths.is_empty() && has_global_submodule_active(&config) {
        selected.retain(|submodule| submodule_is_active(&config, submodule));
    }
    // git iterates the whole selected list and only stops early on a *fatal*
    // failure of the update itself (merge conflict, rebase error, command
    // failure). A plain checkout error continues to the next submodule (so
    // sibling submodules are still updated) but the command's final exit code is
    // non-zero. We thread both behaviors: `first_error` records a non-fatal
    // checkout failure, `?` propagates a fatal one immediately.
    let mut first_error: Option<GitError> = None;
    for submodule in selected {
        let Some(target_oid) = submodule_index_oid(&index, &submodule.path) else {
            // An unmerged (stage > 0) gitlink is skipped with a notice — git's
            // `prepare_to_clone_next_submodule` "Skipping unmerged submodule".
            if submodule_index_is_unmerged(&index, &submodule.path) {
                let display = submodule_displaypath(
                    &cwd,
                    &worktree_root,
                    &submodule.path,
                    &options.super_prefix,
                )?;
                eprintln!("Skipping unmerged submodule {display}");
            }
            continue;
        };
        match update_one_submodule(
            &cwd,
            &git_dir,
            &worktree_root,
            &config,
            submodule,
            &target_oid,
            &options,
        )? {
            UpdateOutcome::Done => {}
            UpdateOutcome::NonFatalCheckoutError(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// The result of updating one submodule. A non-fatal checkout failure is carried
/// up so the loop continues to the next submodule but the overall command still
/// exits non-zero (git's `run_update_command` returns the `git checkout` exit
/// code without `die()`ing the whole run).
enum UpdateOutcome {
    Done,
    NonFatalCheckoutError(GitError),
}

/// The single `submodule update` primitive — git's `update_submodule` +
/// `run_update_procedure` + `run_update_command`. Every update mode
/// (checkout/merge/rebase/command) and every `none`-skip flows through here, so
/// the whole class shares one strategy resolver + one dispatch.
#[allow(clippy::too_many_arguments)]
fn update_one_submodule(
    cwd: &Path,
    git_dir: &Path,
    worktree_root: &Path,
    config: &GitConfig,
    submodule: &SubmoduleConfigEntry,
    target_oid: &ObjectId,
    options: &SubmoduleUpdateOptions<'_>,
) -> Result<UpdateOutcome> {
    let display =
        submodule_displaypath(cwd, worktree_root, &submodule.path, &options.super_prefix)?;
    let path = worktree_root.join(&submodule.path);

    // git's `prepare_to_clone_next_submodule`: an update=none submodule (from
    // .git/config OR .gitmodules) is skipped BEFORE the initialized check, with
    // a "Skipping submodule '<displaypath>'" notice — UNLESS the CLI forced a
    // mode (`--checkout`/`--merge`/`--rebase`), which overrides update=none.
    if options.cli_default == sley_submodule::UpdateType::Unspecified {
        let config_none = config
            .get("submodule", Some(&submodule.name), "update")
            .map(|v| v == "none");
        let effective_none = match config_none {
            Some(is_none) => is_none,
            None => submodule.update.as_deref() == Some("none"),
        };
        if effective_none {
            eprintln!("Skipping submodule '{display}'");
            return Ok(UpdateOutcome::Done);
        }
    }

    // git's `prepare_to_clone_next_submodule`: an INACTIVE submodule is the
    // only one skipped here — with the two-line "not initialized" nudge when an
    // explicit pathspec named it (`warn_if_uninitialized`), silent otherwise.
    if !submodule_is_active(config, submodule) {
        if !options.paths.is_empty() {
            eprintln!("Submodule path '{display}' not initialized");
            eprintln!("Maybe you want to use 'update --init'?");
        }
        return Ok(UpdateOutcome::Done);
    }
    // Active: resolve the clone url from `.git/config`, falling back to the
    // (relative-resolved) `.gitmodules` url. git then `die`s immediately if no
    // url resolves — BEFORE the populated/needs-cloning check — so an active
    // submodule whose url was unset fails loudly rather than being skipped.
    let url = config
        .get("submodule", Some(&submodule.name), "url")
        .map(str::to_string)
        .or_else(|| {
            submodule
                .url
                .as_deref()
                .map(|url| resolve_submodule_init_url(worktree_root, config, url))
        });
    let Some(url) = url.filter(|url| !url.is_empty()) else {
        eprintln!(
            "fatal: cannot clone submodule '{}' without a URL",
            submodule.name
        );
        return Err(GitError::Exit(128));
    };

    let just_populated = submodule_head(&path).is_err();
    if just_populated {
        populate_submodule_worktree(git_dir, submodule, &path, &url, options)?;
    }

    // Resolve the effective update strategy via the single resolver. The typed
    // `.gitmodules` strategy is reconstructed from the raw string the config
    // reader carries.
    let gitmodules_strategy = submodule
        .update
        .as_deref()
        .and_then(sley_submodule::parse_update_strategy)
        .unwrap_or_default();
    let config_update = config.get("submodule", Some(&submodule.name), "update");
    let strategy = match sley_submodule::determine_update_strategy(
        options.cli_default,
        config_update,
        &gitmodules_strategy,
        just_populated,
    ) {
        Ok(strategy) => strategy,
        Err(value) => {
            eprintln!(
                "fatal: Invalid update mode '{value}' configured for submodule path '{}'",
                submodule.path
            );
            return Err(GitError::Exit(128));
        }
    };

    let (sub_git_dir, head_oid) = submodule_head(&path)?;
    let sub_format = repository_object_format(&sub_git_dir)?;

    // `--remote`: re-point the target oid at the upstream branch tip rather than
    // the gitlink recorded in the superproject index. git fetches, then resolves
    // `refs/remotes/<remote>/<branch>`.
    let target_oid = if options.remote {
        remote_target_oid(
            &path,
            &sub_git_dir,
            sub_format,
            config,
            submodule,
            options,
            &display,
        )?
    } else {
        *target_oid
    };

    // `subforce`: git forces the checkout when the submodule has no current HEAD
    // (just cloned) or `--force` was given.
    let subforce = just_populated || options.force;

    // git: run the update only when the target differs from the current HEAD, OR
    // --force was given. A just-populated worktree always needs the checkout.
    if just_populated || head_oid != target_oid || options.force {
        match run_submodule_update_command(
            &path,
            &sub_git_dir,
            &strategy,
            &target_oid,
            subforce,
            options.quiet,
            &display,
        )? {
            UpdateOutcome::Done => {}
            other => return Ok(other),
        }
    }

    // `submodule update --recursive`: after this submodule is checked out,
    // recurse into ITS submodules. Self-invoke `sley submodule update --init
    // --recursive` inside the submodule worktree, which is where the nested
    // `.gitmodules` + `.git/config` (and the nested `.git/modules/...`) live.
    // Running it as a child process — exactly as git's `update_submodule` does —
    // makes the nested git-dir land in `<this>/.git/modules/<sub>` (which, via
    // the gitdir-file chain, is `super/.git/modules/<path>/modules/<sub>`).
    if options.recursive {
        return recurse_submodule_update(&path, &display, options);
    }

    Ok(UpdateOutcome::Done)
}

/// Populate a just-initialized submodule's worktree: reconnect to a retained
/// `.git/modules/<path>` git dir when one exists (upstream `clone_submodule`
/// does the same after the worktree was removed), otherwise clone fresh.
fn populate_submodule_worktree(
    git_dir: &Path,
    submodule: &SubmoduleConfigEntry,
    path: &Path,
    url: &str,
    options: &SubmoduleUpdateOptions<'_>,
) -> Result<()> {
    let modules_git_dir = git_dir.join("modules").join(&submodule.name);
    if modules_git_dir.join("HEAD").is_file() {
        if path.exists() {
            if !path.is_dir() || fs::read_dir(path)?.next().is_some() {
                eprintln!(
                    "fatal: destination path '{}' already exists and is not an empty directory",
                    submodule.path
                );
                return Err(GitError::Exit(128));
            }
        } else {
            fs::create_dir_all(path)?;
        }
        rewrite_submodule_gitdir_file(path, &modules_git_dir)?;
        set_submodule_core_worktree(path, &modules_git_dir)?;
    } else {
        let mut clone_args = Vec::new();
        if options.quiet {
            clone_args.push("-q".to_string());
        }
        if let Some(depth) = options.depth {
            clone_args.push(format!("--depth={depth}"));
        }
        if let Some(filter) = &options.filter {
            clone_args.push(format!("--filter={filter}"));
        }
        clone_args.push("--separate-git-dir".to_string());
        clone_args.push(modules_git_dir.display().to_string());
        clone_args.push(url.to_string());
        clone_args.push(path.display().to_string());
        super::remote_cmds::cmd_clone(&clone_args)?;
        rewrite_submodule_gitdir_file(path, &modules_git_dir)?;
        set_submodule_core_worktree(path, &modules_git_dir)?;
    }
    Ok(())
}

/// `--remote`: fetch the submodule's default remote, then resolve the configured
/// (or `.gitmodules`, or `HEAD`) branch tip to the oid we should update to.
/// Mirrors git's `update_submodule` remote branch: `remote_submodule_branch`
/// (config override > `.gitmodules` branch > `HEAD`, with `.` = superproject's
/// current branch) then `refs/remotes/<remote>/<branch>`.
fn remote_target_oid(
    path: &Path,
    sub_git_dir: &Path,
    sub_format: ObjectFormat,
    config: &GitConfig,
    submodule: &SubmoduleConfigEntry,
    options: &SubmoduleUpdateOptions<'_>,
    display: &str,
) -> Result<ObjectId> {
    let remote = submodule_default_remote(sub_git_dir, sub_format)?;
    let branch = resolve_remote_branch(config, submodule)?;
    let remote_ref = format!("refs/remotes/{remote}/{branch}");

    if !options.nofetch {
        // Run `sley fetch <remote>` inside the submodule worktree so its own
        // config/remote is used (git spawns `git -C <sm_path> fetch`).
        let status = self_sley_fetch(path, &remote)?;
        if !status.success() {
            eprintln!("fatal: Unable to fetch in submodule path '{display}'");
            return Err(GitError::Exit(128));
        }
    }

    let store = FileRefStore::new(sub_git_dir, sub_format);
    if let Some(oid) = resolve_ref_to_oid(&store, &remote_ref)? {
        return Ok(oid);
    }
    eprintln!(
        "fatal: Unable to find {remote_ref} revision in submodule path '{}'",
        display
    );
    Err(GitError::Exit(128))
}

/// Run `sley fetch <remote>` inside the submodule worktree (git's `git -C
/// <sm_path> fetch <remote>`).
fn self_sley_fetch(path: &Path, remote: &str) -> Result<std::process::ExitStatus> {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"));
    let mut command = ProcessCommand::new(exe);
    command.arg("fetch").arg(remote);
    command
        .current_dir(path)
        .status()
        .map_err(|err| GitError::Io(err.to_string()))
}

/// git's `remote_submodule_branch`: `submodule.<name>.branch` from
/// `.git/config`, falling back to the `.gitmodules` branch, then `HEAD`. A `.`
/// value means "inherit the superproject's current branch".
fn resolve_remote_branch(config: &GitConfig, submodule: &SubmoduleConfigEntry) -> Result<String> {
    let branch = config
        .get("submodule", Some(&submodule.name), "branch")
        .map(str::to_string)
        .or_else(|| submodule_gitmodules_branch(submodule));
    let Some(branch) = branch else {
        return Ok("HEAD".to_string());
    };
    if branch != "." {
        return Ok(branch);
    }
    // `.` inherits the superproject's current branch.
    let cwd = env::current_dir()?;
    let super_git_dir = discover_git_dir(&cwd)?;
    let super_format = repository_object_format(&super_git_dir)?;
    let store = FileRefStore::new(&super_git_dir, super_format);
    if let Some(RefTarget::Symbolic(target)) = store.read_ref("HEAD")? {
        if let Some(name) = target.strip_prefix("refs/heads/") {
            return Ok(name.to_string());
        }
    }
    eprintln!(
        "fatal: Submodule ({}) branch configured to inherit branch from superproject, but the superproject is not on any branch",
        submodule.name
    );
    Err(GitError::Exit(128))
}

/// Read the `.gitmodules` `branch` value for a submodule (the typed config
/// reader does not surface it, so re-read it here).
fn submodule_gitmodules_branch(submodule: &SubmoduleConfigEntry) -> Option<String> {
    let cwd = env::current_dir().ok()?;
    let git_dir = discover_git_dir(&cwd).ok()?;
    let worktree_root = worktree_root_for_git_dir(&git_dir).ok()?;
    let gitmodules = GitConfig::read(worktree_root.join(".gitmodules")).ok()?;
    gitmodules
        .get("submodule", Some(&submodule.name), "branch")
        .map(str::to_string)
}

/// git's `get_default_remote_submodule`: the remote of the submodule's current
/// branch, falling back to `origin`.
fn submodule_default_remote(sub_git_dir: &Path, sub_format: ObjectFormat) -> Result<String> {
    let config = read_repo_config(sub_git_dir)?;
    let store = FileRefStore::new(sub_git_dir, sub_format);
    if let Some(RefTarget::Symbolic(target)) = store.read_ref("HEAD")? {
        if let Some(branch) = target.strip_prefix("refs/heads/") {
            if let Some(remote) = config.get("branch", Some(branch), "remote") {
                return Ok(remote.to_string());
            }
        }
    }
    Ok("origin".to_string())
}

/// True when the index carries an unmerged (stage > 0) gitlink at `path`.
fn submodule_index_is_unmerged(index: &Option<Index>, path: &str) -> bool {
    use sley_index::Stage;
    let path = path.as_bytes();
    index.as_ref().is_some_and(|index| {
        index
            .entries
            .iter()
            .any(|entry| entry.path == path && entry.stage() != Stage::Normal)
    })
}

/// Run ONE update strategy against the submodule worktree — git's
/// `run_update_command`. checkout/rebase/merge route through the matching `sley`
/// subcommand (run as a child process inside the submodule, exactly like git
/// spawns `git checkout`/`git rebase`/`git merge`), and a `!command` runs the
/// shell command. The byte-for-byte error text comes from the underlying sley
/// command; we add git's `Unable to … in submodule path` / `Execution of …
/// failed` fatal line on failure and the success notice otherwise.
#[allow(clippy::too_many_arguments)]
fn run_submodule_update_command(
    path: &Path,
    sub_git_dir: &Path,
    strategy: &sley_submodule::UpdateStrategy,
    target_oid: &ObjectId,
    subforce: bool,
    quiet: bool,
    display: &str,
) -> Result<UpdateOutcome> {
    use sley_submodule::UpdateType;
    let oid = target_oid.to_string();

    // Build the subcommand (sley) or shell command to run in the submodule.
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"));
    let mut command;
    match strategy.kind {
        UpdateType::Checkout => {
            command = ProcessCommand::new(&exe);
            command.arg("checkout").arg("-q");
            if subforce {
                command.arg("-f");
            }
            command.arg(&oid);
        }
        UpdateType::Rebase => {
            command = ProcessCommand::new(&exe);
            command.arg("rebase");
            if quiet {
                command.arg("--quiet");
            }
            command.arg(&oid);
        }
        UpdateType::Merge => {
            command = ProcessCommand::new(&exe);
            command.arg("merge");
            if quiet {
                command.arg("--quiet");
            }
            command.arg(&oid);
        }
        UpdateType::Command => {
            let cmd = strategy.command.clone().unwrap_or_default();
            command = ProcessCommand::new("sh");
            command
                .arg("-c")
                .arg(format!("{cmd} \"$@\""))
                .arg(&cmd)
                .arg(&oid);
        }
        UpdateType::None | UpdateType::Unspecified => {
            // Resolved strategy never carries these (none is skipped earlier,
            // unspecified defaults to checkout); be defensive and no-op.
            return Ok(UpdateOutcome::Done);
        }
    }

    io::stdout().flush()?;
    let status = command
        .current_dir(path)
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;

    if !status.success() {
        match strategy.kind {
            UpdateType::Checkout => {
                eprintln!("fatal: Unable to checkout '{oid}' in submodule path '{display}'");
                // git returns the `git checkout` exit code (1) WITHOUT die()ing
                // the whole run, so sibling submodules still update (the loop in
                // update_submodules continues on any code != 128).
                return Ok(UpdateOutcome::NonFatalCheckoutError(GitError::Exit(1)));
            }
            UpdateType::Rebase => {
                eprintln!("fatal: Unable to rebase '{oid}' in submodule path '{display}'");
            }
            UpdateType::Merge => {
                eprintln!("fatal: Unable to merge '{oid}' in submodule path '{display}'");
            }
            UpdateType::Command => {
                let cmd = strategy.command.clone().unwrap_or_default();
                eprintln!("fatal: Execution of '{cmd} {oid}' failed in submodule path '{display}'");
            }
            UpdateType::None | UpdateType::Unspecified => {}
        }
        // rebase/merge/command failures are git's `die_message` (exit 128):
        // fatal, stop the whole run immediately.
        return Err(GitError::Exit(128));
    }

    if !quiet {
        match strategy.kind {
            UpdateType::Checkout => {
                println!("Submodule path '{display}': checked out '{oid}'");
            }
            UpdateType::Rebase => {
                println!("Submodule path '{display}': rebased into '{oid}'");
            }
            UpdateType::Merge => {
                println!("Submodule path '{display}': merged in '{oid}'");
            }
            UpdateType::Command => {
                let cmd = strategy.command.clone().unwrap_or_default();
                println!("Submodule path '{display}': '{cmd} {oid}'");
            }
            UpdateType::None | UpdateType::Unspecified => {}
        }
    }
    let _ = sub_git_dir;
    Ok(UpdateOutcome::Done)
}

/// Recurse `submodule update` into the submodule rooted at `submodule_root` by
/// self-invoking `sley submodule update --init --recursive` as a child process
/// in that worktree — git's `update_submodule` recursion. Running as a child
/// (not an in-process `cwd` swap) is what makes the nested submodule git-dir
/// land in `<submodule>/.git/modules/...` (i.e.
/// `super/.git/modules/<path>/modules/<sub>` via the gitdir-file chain) and
/// keeps the displaypaths anchored at the recursion root. On failure git prints
/// `Failed to recurse into submodule path '<displaypath>'` and propagates the
/// child's exit code.
fn recurse_submodule_update(
    submodule_root: &Path,
    display: &str,
    options: &SubmoduleUpdateOptions<'_>,
) -> Result<UpdateOutcome> {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"));
    let mut command = ProcessCommand::new(exe);
    command.arg("submodule");
    if options.quiet {
        command.arg("--quiet");
    }
    command.arg("update").arg("--init").arg("--recursive");
    if options.force {
        command.arg("--force");
    }
    // Carry the displaypath prefix so the child's nested messages read
    // `<this>/<sub>` — git's `--super-prefix=<displaypath>/`.
    command.arg(format!("--super-prefix={display}/"));
    io::stdout().flush()?;
    let status = command
        .current_dir(submodule_root)
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if status.success() {
        return Ok(UpdateOutcome::Done);
    }
    eprintln!("fatal: Failed to recurse into submodule path '{display}'");
    // git's update_submodules loop continues on a non-128 code (a nested
    // checkout error, code 1) but stops immediately on 128 (a nested
    // rebase/merge/command/clone error). Carry the child's exit code so the
    // outer loop preserves that distinction.
    let code = status.code().unwrap_or(128);
    if code == 128 {
        Err(GitError::Exit(128))
    } else {
        Ok(UpdateOutcome::NonFatalCheckoutError(GitError::Exit(code)))
    }
}

fn parse_submodule_update_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleUpdateOptions<'_>> {
    use sley_submodule::UpdateType;
    let mut init = false;
    let mut recursive = false;
    let mut force = false;
    let mut remote = false;
    let mut nofetch = false;
    let mut cli_default = UpdateType::Unspecified;
    let mut depth = None;
    let mut filter = None;
    let mut super_prefix = String::new();
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
            "--recursive" => recursive = true,
            "--force" | "-f" => force = true,
            "--remote" => remote = true,
            // `-N/--no-fetch` skips the fetch step of an update; only meaningful
            // for `--remote` today (the non-remote path has no separate fetch).
            "--no-fetch" | "-N" => nofetch = true,
            // The three update-mode flags force the strategy (git's
            // `update_default`). Last one wins, like git's option parsing.
            "--checkout" => cli_default = UpdateType::Checkout,
            "--merge" => cli_default = UpdateType::Merge,
            "--rebase" => cli_default = UpdateType::Rebase,
            "--recommend-shallow"
            | "--no-recommend-shallow"
            | "--single-branch"
            | "--no-single-branch"
            | "--progress"
            | "--no-progress" => {}
            "--depth" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                depth = Some(value.parse::<u32>().map_err(|_| {
                    eprintln!("fatal: invalid depth '{value}'");
                    GitError::Exit(128)
                })?);
            }
            value if let Some(value) = value.strip_prefix("--depth=") => {
                depth = Some(value.parse::<u32>().map_err(|_| {
                    eprintln!("fatal: invalid depth '{value}'");
                    GitError::Exit(128)
                })?);
            }
            "--jobs" | "-j" => {
                // Parallelism is accepted but ignored (sley updates serially).
                index += 1;
                if args.get(index).is_none() {
                    return submodule_usage();
                }
            }
            value if value.strip_prefix("--jobs=").is_some() => {}
            "--filter" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                filter = Some(value.clone());
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
            value if let Some(value) = value.strip_prefix("--filter=") => {
                filter = Some(value.to_string());
            }
            // Internal: the recursion displaypath prefix (git's --super-prefix).
            value if let Some(value) = value.strip_prefix("--super-prefix=") => {
                super_prefix = value.to_string();
            }
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
        index += 1;
    }
    // git: `--filter` requires `--init` (exit code 129 — usage error).
    if filter.is_some() && !init {
        eprintln!("fatal: --filter can only be used with the --init option");
        return Err(GitError::Exit(129));
    }
    Ok(SubmoduleUpdateOptions {
        init,
        recursive,
        quiet,
        force,
        remote,
        nofetch,
        cli_default,
        depth,
        filter,
        super_prefix,
        paths,
    })
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

fn submodule_add_name(
    worktree_root: &Path,
    path: &str,
    name_override: Option<&str>,
) -> Result<String> {
    if let Some(name) = name_override {
        if !sley_submodule::check_submodule_name(name) {
            eprintln!("fatal: '{name}' is not a valid submodule name");
            return Err(GitError::Exit(128));
        }
        return Ok(name.to_string());
    }
    let gitmodules_path = worktree_root.join(".gitmodules");
    let gitmodules = GitConfig::read(&gitmodules_path).unwrap_or_default();
    let name = submodule_name_for_exact_path(&gitmodules, path).unwrap_or_else(|| path.to_string());
    if !sley_submodule::check_submodule_name(&name) {
        eprintln!("fatal: '{name}' is not a valid submodule name");
        return Err(GitError::Exit(128));
    }
    Ok(name)
}

fn validate_submodule_add_name_available(
    worktree_root: &Path,
    name: &str,
    path: &str,
    force: bool,
) -> Result<()> {
    if force {
        return Ok(());
    }
    let gitmodules_path = worktree_root.join(".gitmodules");
    let gitmodules = GitConfig::read(&gitmodules_path).unwrap_or_default();
    let set = sley_submodule::SubmoduleConfigSet::parse(&gitmodules);
    if let Some(existing) = set.from_name(name)
        && existing.path.is_some()
        && existing.path.as_deref() != Some(path)
    {
        let existing_path = existing.path.as_deref().unwrap_or("");
        eprintln!(
            "fatal: A git directory for '{}' is found locally with remote(s). Name '{}' is already used for path '{}'",
            path, name, existing_path
        );
        return Err(GitError::Exit(128));
    }
    Ok(())
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

pub(crate) fn set_submodule_core_worktree(
    submodule_root: &Path,
    modules_git_dir: &Path,
) -> Result<()> {
    let mut config = read_repo_config(modules_git_dir)?;
    let worktree = relative_path_from_absolute_components(modules_git_dir, submodule_root)?;
    set_config_value(&mut config, "core", None, "worktree", &worktree);
    write_repo_config(modules_git_dir, &config)
}

fn write_submodule_mapping(
    git_dir: &Path,
    worktree_root: &Path,
    path: &str,
    gitmodules_url: &str,
    config_url: &str,
    branch: Option<&str>,
    name_override: Option<&str>,
) -> Result<()> {
    // git records the ORIGINAL (possibly relative) url in `.gitmodules`, but the
    // RESOLVED `realrepo` in `.git/config` — so a `../sub` add stays portable in
    // `.gitmodules` while `.git/config` holds the concrete clone url.
    let gitmodules_path = worktree_root.join(".gitmodules");
    let mut gitmodules = GitConfig::read(&gitmodules_path).unwrap_or_default();
    let name = name_override.map(str::to_string).unwrap_or_else(|| {
        submodule_name_for_exact_path(&gitmodules, path).unwrap_or_else(|| path.to_string())
    });
    set_submodule_config_value(&mut gitmodules, &name, "path", path);
    set_submodule_config_value(&mut gitmodules, &name, "url", gitmodules_url);
    if let Some(branch) = branch {
        set_submodule_config_value(&mut gitmodules, &name, "branch", branch);
    }
    fs::write(&gitmodules_path, gitmodules.to_canonical_bytes())?;

    let mut config = read_repo_config(git_dir)?;
    set_submodule_config_value(&mut config, &name, "url", config_url);
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
    // git's `configure_added_submodule` stages `.gitmodules` with
    // `git add --force -- .gitmodules` unconditionally, so a repository that
    // gitignores `.gitmodules` (or the whole submodule path under `--force`)
    // still registers the new submodule rather than tripping the ignored-file
    // guard. Mirror that force exactly.
    super::plumbing::cmd_add(&[
        "--force".to_string(),
        "--".to_string(),
        worktree_root.join(".gitmodules").display().to_string(),
    ])?;
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
    let (paths, quiet, super_prefix) = parse_submodule_init_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;

    // git's `module_list_compute` lists the INDEX gitlinks, then `init_submodule`
    // dies "No url found for submodule path '<p>' in .gitmodules" for any listed
    // gitlink with no `.gitmodules` url. Cross-reference the index gitlinks
    // (restricted to the requested pathspecs) against the parsed `.gitmodules`
    // so an index gitlink with no mapping aborts init, matching git.
    let index = read_repository_index(&git_dir, format)?;
    let index_gitlinks = index_relevant_paths(&index, &BTreeMap::new());
    let known_paths: BTreeSet<String> = submodules.iter().map(|s| s.path.clone()).collect();
    for (path, (mode, _)) in &index_gitlinks {
        if *mode != 0o160000 {
            continue;
        }
        if !path_selected_by_specs(&cwd, &worktree_root, path, &paths) {
            continue;
        }
        if !known_paths.contains(path) {
            eprintln!("fatal: No url found for submodule path '{path}' in .gitmodules");
            return Err(GitError::Exit(128));
        }
    }

    let mut config = read_repo_config(&git_dir)?;
    let mut selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &paths)?;
    if paths.is_empty() && has_global_submodule_active(&config) {
        selected.retain(|submodule| submodule_is_active(&config, submodule));
    }
    let mut changed = false;
    for submodule in selected {
        // git's `init_submodule` copies the url and the update setting in two
        // INDEPENDENT guards — each is copied only when not already present in
        // `.git/config`. (The old single-guard form skipped the update copy for
        // an already-url-registered submodule; t7406 #29/#30/#35 re-init after
        // editing `.gitmodules update`.)
        // git's `init_submodule` sets the active flag in an INDEPENDENT guard:
        // any submodule not already active (via `submodule.<name>.active` or the
        // `submodule.active` pathspec) gets `submodule.<name>.active=true`, even
        // when its url is already registered. (sley previously nested this in
        // the url-copy guard, so a url-registered-but-inactive submodule — e.g.
        // one excluded from a `clone --recurse-submodules=<pathspec>` — never
        // got its active flag, breaking the `submodule.<name>.active` query and
        // the active+no-url `submodule update` guard.)
        if !submodule_is_active(&config, submodule) {
            set_submodule_config_value(&mut config, &submodule.name, "active", "true");
            changed = true;
        }
        if config
            .get("submodule", Some(&submodule.name), "url")
            .is_none()
        {
            let Some(url) = &submodule.url else {
                let display =
                    submodule_displaypath(&cwd, &worktree_root, &submodule.path, &super_prefix)?;
                eprintln!("fatal: No url found for submodule path '{display}' in .gitmodules");
                return Err(GitError::Exit(128));
            };
            let url = resolve_submodule_init_url(&worktree_root, &config, url);
            set_submodule_config_value(&mut config, &submodule.name, "url", &url);
            if !quiet {
                let display =
                    submodule_displaypath(&cwd, &worktree_root, &submodule.path, &super_prefix)?;
                eprintln!(
                    "Submodule '{}' ({}) registered for path '{}'",
                    submodule.name, url, display
                );
            }
            changed = true;
        }

        // Copy the `update` setting when unset. A `!command` from `.gitmodules`
        // never reaches here (read_submodule_configs already fatal'd on it), so
        // `submodule.update` only holds checkout/merge/rebase/none — copy it
        // verbatim, matching git's `submodule_update_type_to_string` path.
        if config
            .get("submodule", Some(&submodule.name), "update")
            .is_none()
            && let Some(update) = &submodule.update
        {
            set_submodule_config_value(&mut config, &submodule.name, "update", update);
            changed = true;
        }
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

/// True when `path` is selected by `specs` (all paths when `specs` is empty),
/// using the same normalization as the submodule pathspec matcher.
fn path_selected_by_specs(cwd: &Path, worktree_root: &Path, path: &str, specs: &[&str]) -> bool {
    if specs.is_empty() {
        return true;
    }
    specs.iter().any(|spec| {
        let normalized = normalize_submodule_pathspec(cwd, worktree_root, spec);
        submodule_path_matches_pathspec(path, &normalized)
    })
}

fn cmd_submodule_deinit(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_deinit_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    if submodules.is_empty() {
        return Ok(());
    }
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
        let configured_url = config
            .get("submodule", Some(&submodule.name), "url")
            .map(str::to_string)
            .or_else(|| submodule.url.clone());
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
        let submodule_root = worktree_root.join(&submodule.path);
        if submodule_root.join(".git").is_dir() {
            absorb_submodule_git_dir(&git_dir, &worktree_root, submodule, true)?;
        }
        let had_worktree_dir = submodule_root.is_dir();
        clear_submodule_worktree(&submodule_root)?;
        unset_submodule_core_worktree(&git_dir, submodule)?;
        let had_config = config.sections.iter().any(|section| {
            section.name == "submodule" && section.subsection.as_deref() == Some(&submodule.name)
        });
        remove_submodule_config_section(&mut config, &submodule.name);
        if !options.quiet {
            let display = display_submodule_path(&cwd, &worktree_root, &submodule.path)?;
            if had_worktree_dir {
                println!("Cleared directory '{display}'");
            }
            if had_config {
                let url = configured_url.unwrap_or_default();
                println!(
                    "Submodule '{}' ({}) unregistered for path '{}'",
                    submodule.name, url, display
                );
            }
        }
        changed = true;
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

fn cmd_submodule_sync(args: &[String], quiet: bool) -> Result<()> {
    let (paths, quiet, recursive, super_prefix) = parse_submodule_sync_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &paths)?;
    let mut config = read_repo_config(&git_dir)?;
    let mut changed = false;
    for submodule in selected {
        sync_one_submodule(
            &git_dir,
            &worktree_root,
            &cwd,
            &mut config,
            &mut changed,
            submodule,
            quiet,
            recursive,
            &super_prefix,
        )?;
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

/// Port of `sync_submodule` (`builtin/submodule--helper.c`). For an active
/// submodule: re-derive the superproject `.git/config` url and (when populated)
/// the submodule's own `remote.<default>.url` from the `.gitmodules` url —
/// resolving the latter with the worktree-relative `up_path` so a relative url
/// stays relative to the submodule — then optionally recurse.
#[allow(clippy::too_many_arguments)]
fn sync_one_submodule(
    git_dir: &Path,
    worktree_root: &Path,
    cwd: &Path,
    config: &mut GitConfig,
    changed: &mut bool,
    submodule: &SubmoduleConfigEntry,
    quiet: bool,
    recursive: bool,
    super_prefix: &str,
) -> Result<()> {
    // `is_submodule_active`: sync only touches submodules registered in
    // `.git/config` (a `submodule.<name>.url`).
    if config
        .get("submodule", Some(&submodule.name), "url")
        .is_none()
    {
        return Ok(());
    }

    // git derives TWO urls: `super_config_url` (no up_path) for the
    // superproject's `.git/config`, and `sub_origin_url` (with up_path) for the
    // submodule's own remote — a relative url must stay relative to the deeper
    // submodule worktree. A non-relative url is used verbatim for both.
    let up_path = submodule_up_path(&submodule.path);
    let (super_config_url, sub_origin_url) = match &submodule.url {
        Some(url) if url.starts_with("../") || url.starts_with("./") => {
            resolve_sync_urls(worktree_root, config, url, &up_path)
        }
        Some(url) => (url.clone(), url.clone()),
        None => (String::new(), String::new()),
    };

    let display = submodule_displaypath(cwd, worktree_root, &submodule.path, super_prefix)?;
    if !quiet {
        println!("Synchronizing submodule url for '{display}'");
    }
    set_submodule_config_value(config, &submodule.name, "url", &super_config_url);
    *changed = true;

    // Only a populated submodule has a git dir whose remote we can update.
    let submodule_root = worktree_root.join(&submodule.path);
    let Some(sub_git_dir) = resolve_submodule_repo_gitdir(&submodule_root) else {
        return Ok(());
    };
    let sub_format = repository_object_format(&sub_git_dir)?;
    let mut sub_config = read_repo_config(&sub_git_dir)?;
    let abs_path = normalize_lexical_path(&submodule_root);
    let remote = submodule_default_remote_name(
        worktree_root,
        git_dir,
        &abs_path,
        &sub_git_dir,
        sub_format,
        &sub_config,
    )?;
    set_config_value(&mut sub_config, "remote", Some(&remote), "url", &sub_origin_url);
    write_repo_config(&sub_git_dir, &sub_config)?;

    if recursive {
        recurse_submodule_sync(&submodule_root, &display, quiet)?;
    }
    Ok(())
}

/// `get_up_path`: one `../` per component of the submodule's worktree-relative
/// path — the prefix that takes the submodule worktree back to the superproject.
fn submodule_up_path(path: &str) -> String {
    let components = path.split('/').filter(|part| !part.is_empty()).count();
    "../".repeat(components)
}

/// Resolve a relative `.gitmodules` url against the superproject's default-remote
/// url to the `(super_config_url, sub_origin_url)` pair git records. `sync`
/// suppresses the missing-remote warning, so both go straight through the
/// `relative_url` primitive (no `init`-style warning).
fn resolve_sync_urls(
    worktree_root: &Path,
    config: &GitConfig,
    url: &str,
    up_path: &str,
) -> (String, String) {
    let (_remote, base) = superproject_remote_base(worktree_root, config);
    let base = base.as_deref();
    let cwd_fallback = worktree_root.to_string_lossy();
    let super_config_url = sley_submodule::resolve_relative_url(url, base, &cwd_fallback, None);
    let sub_origin_url =
        sley_submodule::resolve_relative_url(url, base, &cwd_fallback, Some(up_path));
    (super_config_url, sub_origin_url)
}

/// Recurse `submodule sync --recursive` into the populated submodule at
/// `submodule_root`, carrying `--super-prefix=<display>/` so nested displaypaths
/// anchor at the recursion root (git's `sync_submodule` `OPT_RECURSIVE` branch).
fn recurse_submodule_sync(submodule_root: &Path, display: &str, quiet: bool) -> Result<()> {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"));
    let mut command = ProcessCommand::new(exe);
    command.arg("submodule");
    if quiet {
        command.arg("--quiet");
    }
    command.arg("sync").arg("--recursive");
    command.arg(format!("--super-prefix={display}/"));
    io::stdout().flush()?;
    let status = command
        .current_dir(submodule_root)
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if status.success() {
        return Ok(());
    }
    eprintln!("fatal: failed to recurse into submodule '{display}'");
    Err(GitError::Exit(128))
}

fn cmd_submodule_absorbgitdirs(args: &[String], quiet: bool) -> Result<()> {
    let (paths, quiet, super_prefix) = parse_submodule_absorbgitdirs_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    // git's `module_list_compute`: iterate EVERY gitlink in the index (in index
    // order), not just `.gitmodules`-configured ones — a gitlink with no mapping
    // must still be visited so absorb can die "could not lookup name".
    let gitlink_paths = index_gitlink_paths(&git_dir, format, &cwd, &worktree_root, &paths)?;
    let configured = read_submodule_configs(&worktree_root)?;
    for path in gitlink_paths {
        let name = configured
            .iter()
            .find(|sub| sub.path == path)
            .map(|sub| sub.name.as_str());
        absorb_git_dir_into_superproject(
            &git_dir,
            &worktree_root,
            &path,
            name,
            &super_prefix,
            quiet,
        )?;
    }
    Ok(())
}

/// `module_list_compute` for absorb: the worktree-relative paths of all gitlink
/// index entries, de-duplicated across stages, filtered by the pathspec.
fn index_gitlink_paths(
    git_dir: &Path,
    format: ObjectFormat,
    cwd: &Path,
    worktree_root: &Path,
    paths: &[&str],
) -> Result<Vec<String>> {
    let Some(index) = read_repository_index(git_dir, format)? else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in &index.entries {
        if !sley_index::is_gitlink(entry.mode) {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&entry.path) else {
            continue;
        };
        if !seen.insert(path.to_string()) {
            continue;
        }
        if !paths.is_empty()
            && !paths.iter().any(|spec| {
                let normalized = normalize_submodule_pathspec(cwd, worktree_root, spec);
                submodule_path_matches_pathspec(path, &normalized)
            })
        {
            continue;
        }
        result.push(path.to_string());
    }
    Ok(result)
}

/// Port of `absorb_git_dir_into_superproject` (`submodule.c`): migrate an
/// embedded `<path>/.git` directory into the superproject, re-link a gitfile that
/// went stale after the superproject itself was absorbed, then recurse into the
/// submodule for nested gitlinks. `name` is the `.gitmodules` mapping (None when
/// the gitlink is unmapped — fatal only when a migration is actually needed).
fn absorb_git_dir_into_superproject(
    git_dir: &Path,
    worktree_root: &Path,
    path: &str,
    name: Option<&str>,
    super_prefix: &str,
    quiet: bool,
) -> Result<()> {
    let sub_root = worktree_root.join(path);
    let dot_git = sub_root.join(".git");
    let display = format!("{super_prefix}{path}");

    if dot_git.is_dir() {
        // Populated with an embedded git dir. Already inside the superproject's
        // git dir? Then it is absorbed; otherwise relocate it.
        let common = common_git_dir_for_git_dir(git_dir)?;
        let real_dot = fs::canonicalize(&dot_git)?;
        let real_common = fs::canonicalize(&common)?;
        if !real_dot.starts_with(&real_common) {
            let Some(name) = name else {
                eprintln!("fatal: could not lookup name for submodule '{path}'");
                return Err(GitError::Exit(128));
            };
            relocate_submodule_git_dir(git_dir, &sub_root, name, &display, quiet)?;
        }
    } else if dot_git.is_file() {
        // A gitfile that no longer resolves (the superproject moved and the link
        // was not rewritten) must be re-pointed at the relocated git dir.
        if !gitfile_resolves(&dot_git) {
            let Some(name) = name else {
                eprintln!("fatal: could not lookup name for submodule '{path}'");
                return Err(GitError::Exit(128));
            };
            let modules_git_dir = git_dir.join("modules").join(name);
            connect_work_tree_and_git_dir(&sub_root, &modules_git_dir)?;
        }
    } else {
        // Unpopulated (no `.git`): nothing to absorb and nothing to recurse into.
        return Ok(());
    }

    recurse_submodule_absorbgitdirs(&sub_root, &display, quiet)?;
    Ok(())
}

/// Whether a `gitdir:` pointer file resolves to an existing git directory.
fn gitfile_resolves(dot_git: &Path) -> bool {
    matches!(read_gitdir_file(dot_git), Ok(Some(target)) if is_git_dir_candidate(&target))
}

/// git's `relocate_single_git_dir_into_superproject`: move the embedded
/// `<sub>/.git` dir to `<git_dir>/modules/<name>`, print the migration banner,
/// then re-link the worktree (`connect_work_tree_and_git_dir`).
fn relocate_submodule_git_dir(
    git_dir: &Path,
    sub_root: &Path,
    name: &str,
    display: &str,
    quiet: bool,
) -> Result<()> {
    let dot_git = sub_root.join(".git");
    let modules_git_dir = git_dir.join("modules").join(name);
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
        eprintln!("Migrating git directory of '{display}' from");
        eprintln!("'{}' to", from_display.display());
        eprintln!("'{}'", to_display.display());
    }
    fs::rename(&dot_git, &modules_git_dir)?;
    connect_work_tree_and_git_dir(sub_root, &modules_git_dir)
}

/// git's `connect_work_tree_and_git_dir`: write the `<sub>/.git` gitfile pointing
/// at the (now relocated) git dir and record the matching `core.worktree`.
fn connect_work_tree_and_git_dir(sub_root: &Path, modules_git_dir: &Path) -> Result<()> {
    let dot_git = sub_root.join(".git");
    let gitdir_link = relative_path_from_absolute_components(sub_root, modules_git_dir)?;
    fs::write(&dot_git, format!("gitdir: {gitdir_link}\n"))?;
    let mut config = read_repo_config(modules_git_dir)?;
    let worktree = relative_path_from_absolute_components(modules_git_dir, sub_root)?;
    set_config_value(&mut config, "core", None, "worktree", &worktree);
    write_repo_config(modules_git_dir, &config)
}

/// git's `absorb_git_dir_into_superproject_recurse`: self-invoke
/// `submodule absorbgitdirs --super-prefix=<display>/` inside the populated
/// submodule so nested gitlinks are absorbed with anchored displaypaths.
fn recurse_submodule_absorbgitdirs(sub_root: &Path, display: &str, quiet: bool) -> Result<()> {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"));
    let mut command = ProcessCommand::new(exe);
    command.arg("submodule");
    if quiet {
        command.arg("--quiet");
    }
    command.arg("absorbgitdirs");
    command.arg(format!("--super-prefix={display}/"));
    io::stdout().flush()?;
    let status = command
        .current_dir(sub_root)
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if status.success() {
        return Ok(());
    }
    let code = status.code().unwrap_or(128);
    Err(GitError::Exit(code))
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

/// A single gitlink change to summarize, the analogue of git's `struct
/// module_cb`. Built by diffing the source tree (HEAD or a given commit) against
/// the index (default / `--cached`) or, for `--files`, the index against the
/// worktree. `summary` iterates over these, mirroring git's
/// `compute_summary_module_list` + `submodule_summary_callback`.
struct SubmoduleSummaryEntry {
    sm_path: String,
    /// `'A'` add, `'D'` delete, `'M'` modify, `'T'` type-change.
    status: char,
    mod_src: u32,
    mod_dst: u32,
    oid_src: ObjectId,
    oid_dst: ObjectId,
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
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);

    // module_summary: the leading operand is the `<commit>` ONLY if it resolves
    // to an object (git's `repo_get_oid`); otherwise it's a pathspec and the
    // source side is HEAD. A leading "HEAD" before the first commit resolves to
    // the empty tree.
    let mut commit: Option<String> = None;
    let mut positionals = options.positionals.clone();
    if let Some(candidate) = &options.commit {
        if candidate == "HEAD" || resolve_revision(&git_dir, format, candidate).is_ok() {
            commit = Some(candidate.clone());
        } else {
            // Not a revision: it is the first pathspec.
            positionals.insert(0, candidate.clone());
        }
    }

    // module_summary: resolve the source-side commit. With a commit arg, use it;
    // otherwise HEAD, falling back to the empty tree before the first commit.
    let head_tree = summary_source_tree(&db, &git_dir, format, commit.as_deref())?;

    let index = read_repository_index(&git_dir, format)?;
    let mut entries = if options.files {
        // --files: diff-files (index vs worktree).
        compute_summary_diff_files(&worktree_root, format, &index)?
    } else {
        // default / --cached: diff-index (source tree vs index).
        compute_summary_diff_index(&worktree_root, format, &head_tree, &index, options.cached)?
    };

    // Restrict to the requested pathspecs (git passes them through to diff).
    if !positionals.is_empty() {
        let specs: Vec<String> = positionals
            .iter()
            .map(|path| normalize_submodule_pathspec(&cwd, &worktree_root, path))
            .collect();
        entries.retain(|entry| {
            specs
                .iter()
                .any(|spec| submodule_path_matches_pathspec(&entry.sm_path, spec))
        });
    }

    for entry in &entries {
        generate_submodule_summary(
            &cwd,
            &worktree_root,
            &index,
            entry,
            options.cached,
            options.summary_limit,
        )?;
    }
    Ok(())
}

/// `module_summary`'s source-OID resolution: a tree of `(path -> (mode, oid))`
/// for every gitlink in the source side. `commit` is the optional positional;
/// `None` means HEAD (or the empty tree before the first commit). Returns the
/// flattened gitlink set keyed by path.
fn summary_source_tree(
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    commit: Option<&str>,
) -> Result<BTreeMap<String, (u32, ObjectId)>> {
    let tree_oid = match commit {
        Some(rev) => match resolve_revision(git_dir, format, rev) {
            Ok(oid) => Some(commit_tree_oid(db, format, &oid)?),
            // git: a bad rev that isn't "HEAD" dies; "HEAD" before first commit
            // falls back to the empty tree. We treat an unresolvable rev as the
            // empty tree, which matches the no-commits-yet case the tests hit.
            Err(_) => None,
        },
        None => match resolve_revision(git_dir, format, "HEAD") {
            Ok(oid) => Some(commit_tree_oid(db, format, &oid)?),
            Err(_) => None,
        },
    };
    match tree_oid {
        Some(tree) => collect_tree_gitlinks(db, format, &tree),
        None => Ok(BTreeMap::new()),
    }
}

fn commit_tree_oid(
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

/// Recursively collect every gitlink (mode 160000) entry in `tree`, keyed by its
/// full slash-joined path. This is the source side of the summary's gitlink diff.
fn collect_tree_gitlinks(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<String, (u32, ObjectId)>> {
    let mut out = BTreeMap::new();
    collect_tree_gitlinks_into(db, format, tree_oid, "", &mut out)?;
    Ok(out)
}

fn collect_tree_gitlinks_into(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &str,
    out: &mut BTreeMap<String, (u32, ObjectId)>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let name = String::from_utf8_lossy(entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        match entry.mode {
            0o160000 => {
                out.insert(path, (entry.mode, entry.oid));
            }
            0o040000 => {
                collect_tree_gitlinks_into(db, format, &entry.oid, &path, out)?;
            }
            // A regular blob at a path that is a gitlink on the other side is a
            // type-change; the index/tree walk on the other side records its
            // mode, so capture blobs too (needed for submodule->blob detection).
            _ => {
                out.insert(path, (entry.mode, entry.oid));
            }
        }
    }
    Ok(())
}

/// `diff-index` for summary: pair the source tree (`old`) against the index
/// (`new`). The "new" side is the index modes/oids; in the default (non-cached)
/// mode, a gitlink whose checked-out submodule HEAD has moved off the index oid
/// gets its dst oid refilled from that HEAD, which is how `run_diff_index(0)`
/// surfaces a forward/backward move even after the superproject committed it.
/// `--cached` reads the dst oid straight from the index.
fn compute_summary_diff_index(
    worktree_root: &Path,
    format: ObjectFormat,
    old: &BTreeMap<String, (u32, ObjectId)>,
    index: &Option<Index>,
    cached: bool,
) -> Result<Vec<SubmoduleSummaryEntry>> {
    let mut new = index_relevant_paths(index, old);
    if !cached {
        // Default mode: run_diff_index(0) compares against the WORKTREE.
        // For each relevant path, the dst side is the actual worktree entry:
        // a checked-out embedded repository is a gitlink at its HEAD, a file is
        // the blob on disk, and a missing path is a deletion. This matters for
        // typechanges where the index still records a blob but the worktree has
        // already been replaced by a submodule (or the reverse).
        let mut removed = Vec::new();
        for (path, slot) in new.iter_mut() {
            match summary_worktree_side(worktree_root, format, path)? {
                Some(side) => *slot = side,
                None => removed.push(path.clone()),
            }
        }
        for path in removed {
            new.remove(&path);
        }
    }
    Ok(diff_gitlink_sides(format, old, &new))
}

/// `diff-files` for summary `--files`: index (`old`) vs worktree-HEAD (`new`).
/// The worktree side of a gitlink is the submodule's current HEAD commit; a
/// type-change to a regular file uses the worktree blob's mode.
fn compute_summary_diff_files(
    worktree_root: &Path,
    format: ObjectFormat,
    index: &Option<Index>,
) -> Result<Vec<SubmoduleSummaryEntry>> {
    let old = index_relevant_paths_for_files(worktree_root, format, index)?;
    let mut new = BTreeMap::new();
    for path in old.keys() {
        if let Some(side) = summary_worktree_side(worktree_root, format, path)? {
            new.insert(path.clone(), side);
        }
    }
    Ok(diff_gitlink_sides(format, &old, &new)
        .into_iter()
        .filter(|entry| entry.status == 'M' || entry.status == 'T')
        .collect())
}

fn index_relevant_paths_for_files(
    worktree_root: &Path,
    format: ObjectFormat,
    index: &Option<Index>,
) -> Result<BTreeMap<String, (u32, ObjectId)>> {
    use sley_index::Stage;
    let mut out = BTreeMap::new();
    if let Some(index) = index {
        for entry in &index.entries {
            if entry.stage() != Stage::Normal {
                continue;
            }
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            let worktree_is_gitlink = summary_worktree_side(worktree_root, format, &path)?
                .is_some_and(|(mode, _)| mode == 0o160000);
            if entry.mode == 0o160000 || worktree_is_gitlink {
                out.insert(path, (entry.mode, entry.oid));
            }
        }
    }
    Ok(out)
}

fn summary_worktree_side(
    worktree_root: &Path,
    format: ObjectFormat,
    path: &str,
) -> Result<Option<(u32, ObjectId)>> {
    let worktree_path = worktree_root.join(path);
    if let Ok((_, head_oid)) = submodule_head(&worktree_path) {
        return Ok(Some((0o160000, head_oid)));
    }
    let metadata = match fs::symlink_metadata(&worktree_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return Ok(None);
    }
    if !(file_type.is_file() || file_type.is_symlink()) {
        return Ok(None);
    }
    let body = if file_type.is_symlink() {
        fs::read_link(&worktree_path)?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec()
    } else {
        fs::read(&worktree_path)?
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    let mode = if file_type.is_symlink() {
        0o120000
    } else {
        summary_file_mode(&metadata)
    };
    Ok(Some((mode, oid)))
}

#[cfg(unix)]
fn summary_file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
fn summary_file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

/// The index modes/oids at every stage-0 path that is a gitlink in the index OR
/// present on the `tree_side` (so both `gitlink -> blob` and `blob -> gitlink`
/// type-changes can be captured once the default summary substitutes the
/// worktree side). Keyed by path.
fn index_relevant_paths(
    index: &Option<Index>,
    tree_side: &BTreeMap<String, (u32, ObjectId)>,
) -> BTreeMap<String, (u32, ObjectId)> {
    use sley_index::Stage;
    let mut out = BTreeMap::new();
    if let Some(index) = index {
        for entry in &index.entries {
            if entry.stage() != Stage::Normal {
                continue;
            }
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            if entry.mode == 0o160000 || tree_side.contains_key(&path) {
                out.insert(path, (entry.mode, entry.oid));
            }
        }
    }
    out
}

/// The diff core: pair `old` (source) against `new` (dest) over the union of
/// paths, emitting a [`SubmoduleSummaryEntry`] per changed path where either
/// side is a gitlink. Mirrors `submodule_summary_callback`'s filter
/// (`S_ISGITLINK(one) || S_ISGITLINK(two)`).
fn diff_gitlink_sides(
    format: ObjectFormat,
    old: &BTreeMap<String, (u32, ObjectId)>,
    new: &BTreeMap<String, (u32, ObjectId)>,
) -> Vec<SubmoduleSummaryEntry> {
    let mut paths: BTreeSet<&String> = BTreeSet::new();
    paths.extend(old.keys());
    paths.extend(new.keys());
    let null = ObjectId::null(format);
    let mut entries = Vec::new();
    for path in paths {
        let src = old.get(path).copied();
        let dst = new.get(path).copied();
        let (mod_src, oid_src) = src.unwrap_or((0, null));
        let (mod_dst, oid_dst) = dst.unwrap_or((0, null));
        // Only gitlink-touching pairs matter.
        if mod_src != 0o160000 && mod_dst != 0o160000 {
            continue;
        }
        if mod_src == mod_dst && oid_src == oid_dst {
            continue;
        }
        let status = if mod_src == 0 {
            'A'
        } else if mod_dst == 0 {
            'D'
        } else if mod_src != mod_dst {
            'T'
        } else {
            'M'
        };
        entries.push(SubmoduleSummaryEntry {
            sm_path: path.clone(),
            status,
            mod_src,
            mod_dst,
            oid_src,
            oid_dst,
        });
    }
    entries
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

fn parse_submodule_init_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool, String)> {
    let mut paths = Vec::new();
    let mut super_prefix = String::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            // Internal: the recursion displaypath prefix (git's --super-prefix).
            value if let Some(value) = value.strip_prefix("--super-prefix=") => {
                super_prefix = value.to_string();
            }
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet, super_prefix))
}

struct SubmoduleDeinitOptions<'a> {
    all: bool,
    force: bool,
    quiet: bool,
    paths: Vec<&'a str>,
}

struct SubmoduleForeachOptions {
    /// The raw command argv after option parsing. git keeps the args un-joined
    /// so it can reproduce `run-command`'s two modes: a single arg runs through
    /// the shell (with the per-submodule env vars), multiple args run directly
    /// (no re-evaluation) unless argv[0] itself needs a shell.
    args: Vec<String>,
    quiet: bool,
    recursive: bool,
}

struct SubmoduleSummaryOptions {
    cached: bool,
    files: bool,
    quiet: bool,
    summary_limit: Option<isize>,
    /// The optional `[<commit>]` argument (resolved as the diff source side).
    commit: Option<String>,
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
) -> Result<(Vec<&str>, bool, bool, String)> {
    let mut paths = Vec::new();
    let mut positional_only = false;
    let mut recursive = false;
    let mut super_prefix = String::new();
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
            // Internal: git's `--super-prefix=<path>/`, carried by the recursive
            // child so its nested displaypaths anchor at the recursion root.
            value if value.starts_with("--super-prefix=") => {
                super_prefix = value["--super-prefix=".len()..].to_string();
            }
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet, recursive, super_prefix))
}

fn parse_submodule_absorbgitdirs_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool, String)> {
    let mut paths = Vec::new();
    let mut positional_only = false;
    let mut super_prefix = String::new();
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            // Internal: git's `--super-prefix=<path>/`, carried by the recursive
            // child so its migration messages anchor at the recursion root.
            value if value.starts_with("--super-prefix=") => {
                super_prefix = value["--super-prefix=".len()..].to_string();
            }
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet, super_prefix))
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
            // git's `cmd_foreach` has no `--` end-of-options arm: `--` matches
            // the `-*) usage` case, so `foreach -- <anything>` is a usage error
            // (exit 1). The command is the first non-option token onward.
            value if value.starts_with('-') => return submodule_usage(),
            _ => break,
        }
    }
    Ok(SubmoduleForeachOptions {
        args: args[index..].to_vec(),
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
    // git collects all non-option args first, then peels the leading one off as
    // the `<commit>` if it resolves; the rest are pathspecs.
    let mut operands = Vec::new();
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            operands.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--cached" => cached = true,
            "--files" => files = true,
            // `--for-status` is accepted (it tweaks ignore=all skipping, which
            // sley's diff-driven summary handles structurally) and otherwise
            // behaves like a plain summary.
            "--for-status" => {}
            "--summary-limit" | "-n" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                summary_limit = Some(parse_submodule_summary_limit(value)?);
            }
            value if let Some(value) = value.strip_prefix("--summary-limit=") => {
                summary_limit = Some(parse_submodule_summary_limit(value)?);
            }
            value if let Some(value) = value.strip_prefix("-n") => {
                summary_limit = Some(parse_submodule_summary_limit(value)?);
            }
            value if value.starts_with('-') => return submodule_usage(),
            value => operands.push(value.to_string()),
        }
        index += 1;
    }
    if cached && files {
        eprintln!("fatal: options '--cached' and '--files' cannot be used together");
        return Err(GitError::Exit(128));
    }
    // Peel the leading operand as the commit; the rest are pathspecs. A leading
    // operand of "HEAD" is also consumed as the commit. (git resolves it with
    // repo_get_oid; an unresolvable leading operand still consumes the slot and
    // resolves to the empty tree, matching `nonexistent commit`.)
    let (commit, positionals) = if operands.is_empty() {
        (None, Vec::new())
    } else {
        let mut iter = operands.into_iter();
        let commit = iter.next();
        (commit, iter.collect())
    };
    Ok(SubmoduleSummaryOptions {
        cached,
        files,
        quiet,
        summary_limit,
        commit,
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
    // PILOT (sley-submodule): route the exact-path → name lookup through the
    // typed parser's `from_path`, which is git's `submodule_from_path` keyed on
    // the (first-wins) `path` binding.
    sley_submodule::SubmoduleConfigSet::parse(config)
        .from_path(path)
        .map(|submodule| submodule.name.clone())
}

fn filter_submodule_configs<'a>(
    cwd: &Path,
    worktree_root: &Path,
    submodules: &'a [SubmoduleConfigEntry],
    paths: &[&str],
) -> Result<Vec<&'a SubmoduleConfigEntry>> {
    if paths.is_empty() {
        // git's `module_list_compute` returns submodules sorted by path, not in
        // .gitmodules declaration order; foreach/update/status all depend on
        // that ordering (e.g. `nested1` before `sub1`).
        let mut selected = submodules.iter().collect::<Vec<_>>();
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
        selected.extend(matching);
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    selected.dedup_by(|left, right| left.path == right.path);
    Ok(selected)
}

fn validate_status_submodule_mappings(
    cwd: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodules: &[SubmoduleConfigEntry],
    paths: &[&str],
) -> Result<()> {
    let known_paths = submodules
        .iter()
        .map(|submodule| submodule.path.as_str())
        .collect::<HashSet<_>>();
    let Some(index) = index else {
        return Ok(());
    };
    for entry in &index.entries {
        if entry.stage() != sley_index::Stage::Normal || !sley_index::is_gitlink(entry.mode) {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path);
        if !path_selected_by_specs(cwd, worktree_root, &path, paths) {
            continue;
        }
        if !known_paths.contains(path.as_ref()) {
            eprintln!("fatal: no submodule mapping found in .gitmodules for path '{path}'");
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

fn filter_update_submodule_configs<'a>(
    cwd: &Path,
    worktree_root: &Path,
    submodules: &'a [SubmoduleConfigEntry],
    paths: &[&str],
    index: &Option<Index>,
    super_prefix: &str,
) -> Result<Vec<&'a SubmoduleConfigEntry>> {
    if paths.is_empty() {
        return filter_submodule_configs(cwd, worktree_root, submodules, paths);
    }
    let mut selected = Vec::new();
    for path in paths {
        let normalized = normalize_submodule_pathspec(cwd, worktree_root, path);
        let matching = submodules
            .iter()
            .filter(|submodule| submodule_path_matches_pathspec(&submodule.path, &normalized))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            if index_has_gitlink_matching(index, &normalized) {
                let display = if super_prefix.is_empty() {
                    display_submodule_path(cwd, worktree_root, &normalized)?
                } else {
                    format!("{super_prefix}{normalized}")
                };
                eprintln!("Submodule path '{display}' not initialized");
                eprintln!("Maybe you want to use 'update --init'?");
                continue;
            }
            eprintln!("error: pathspec '{path}' did not match any file(s) known to git");
            return Err(GitError::Exit(1));
        }
        selected.extend(matching);
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    selected.dedup_by(|left, right| left.path == right.path);
    Ok(selected)
}

fn index_has_gitlink_matching(index: &Option<Index>, pathspec: &str) -> bool {
    index.as_ref().is_some_and(|index| {
        index.entries.iter().any(|entry| {
            sley_index::is_gitlink(entry.mode)
                && entry.stage() == sley_index::Stage::Normal
                && submodule_path_matches_pathspec(&String::from_utf8_lossy(&entry.path), pathspec)
        })
    })
}

/// Resolve a `.gitmodules` submodule url to the concrete url recorded in
/// `.git/config`, via the `sley-submodule` `relative_url` primitive (git's
/// `submodule--helper.c::resolve_relative_url`). When the superproject has no
/// `remote.<default>.url`, git warns and falls back to its own worktree root as
/// the authoritative upstream (`xgetcwd()`); `warn_on_missing_remote` mirrors the
/// `init` path's warning, which `sync` suppresses.
fn resolve_submodule_relative_url(
    worktree_root: &Path,
    config: &GitConfig,
    url: &str,
    warn_on_missing_remote: bool,
) -> String {
    // git's `resolve_relative_url` resolves against `remote.<default>.url`, where
    // the default remote is the HEAD branch's remote, else the sole remote, else
    // "origin" (`get_default_remote`). Hardcoding "origin" misresolves a super-
    // project whose only remote was renamed (e.g. a nested submodule whose
    // `origin` is now `upstream`).
    let (remote, base) = superproject_remote_base(worktree_root, config);
    if base.is_none() && warn_on_missing_remote && (url.starts_with("../") || url.starts_with("./"))
    {
        eprintln!(
            "warning: could not look up configuration 'remote.{remote}.url'. Assuming this repository is its own authoritative upstream."
        );
    }
    let cwd_fallback = worktree_root.to_string_lossy();
    sley_submodule::resolve_relative_url(url, base.as_deref(), &cwd_fallback, None)
}

/// The git dir backing the worktree at `worktree_root` (`.git` dir, or the target
/// of a `.git` gitfile), if present.
fn worktree_git_dir(worktree_root: &Path) -> Option<PathBuf> {
    let dot_git = worktree_root.join(".git");
    if dot_git.is_dir() {
        Some(dot_git)
    } else if dot_git.is_file() {
        read_gitdir_file(&dot_git).ok().flatten()
    } else {
        None
    }
}

/// The superproject's default remote NAME (`get_default_remote`) and its
/// `remote.<name>.url`. Remotes and branch config live in the COMMON git dir
/// (shared across linked worktrees), so the lookup reads that config — for a
/// main worktree the common dir IS the git dir, making this identical to the
/// per-worktree config. Falls back to the passed config when the common config
/// can't be read.
fn superproject_remote_base(
    worktree_root: &Path,
    fallback: &GitConfig,
) -> (String, Option<String>) {
    let git_dir = worktree_git_dir(worktree_root);
    let common_config = git_dir
        .as_deref()
        .and_then(|gd| common_git_dir_for_git_dir(gd).ok())
        .and_then(|common| read_repo_config(&common).ok());
    let config = common_config.as_ref().unwrap_or(fallback);
    let remote = match &git_dir {
        Some(git_dir) if repository_object_format(git_dir).is_ok() => {
            let format = repository_object_format(git_dir).unwrap_or(ObjectFormat::Sha1);
            repo_default_remote(config, git_dir, format)
        }
        _ => {
            let remotes = distinct_remote_names(config);
            if remotes.len() == 1 {
                remotes[0].clone()
            } else {
                "origin".to_string()
            }
        }
    };
    let base = config.get("remote", Some(&remote), "url").map(str::to_string);
    (remote, base)
}

fn resolve_submodule_init_url(worktree_root: &Path, config: &GitConfig, url: &str) -> String {
    resolve_submodule_relative_url(worktree_root, config, url, true)
}

fn resolve_submodule_sync_url(worktree_root: &Path, config: &GitConfig, url: &str) -> String {
    resolve_submodule_relative_url(worktree_root, config, url, false)
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

fn unset_submodule_core_worktree(git_dir: &Path, submodule: &SubmoduleConfigEntry) -> Result<()> {
    let modules_git_dir = git_dir.join("modules").join(&submodule.name);
    if !modules_git_dir.join("config").is_file() {
        return Ok(());
    }
    let mut config = read_repo_config(&modules_git_dir)?;
    unset_config_value(&mut config, "core", None, "worktree");
    write_repo_config(&modules_git_dir, &config)
}

fn unset_config_value(config: &mut GitConfig, section: &str, subsection: Option<&str>, key: &str) {
    let Some(section) = config.sections.iter_mut().rev().find(|candidate| {
        candidate.name == section && candidate.subsection.as_deref() == subsection
    }) else {
        return;
    };
    section
        .entries
        .retain(|entry| !entry.key.eq_ignore_ascii_case(key));
}

fn has_global_submodule_active(config: &GitConfig) -> bool {
    !config.get_all("submodule", None, "active").is_empty()
}

fn submodule_is_active(config: &GitConfig, submodule: &SubmoduleConfigEntry) -> bool {
    if let Some(active) = config.get_bool("submodule", Some(&submodule.name), "active") {
        return active;
    }
    let active_specs = config
        .get_all("submodule", None, "active")
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if !active_specs.is_empty() {
        return submodule_active_specs_match(&active_specs, &submodule.path);
    }
    config
        .get("submodule", Some(&submodule.name), "url")
        .is_some()
}

fn submodule_active_specs_match(specs: &[&str], path: &str) -> bool {
    let mut matched = false;
    for spec in specs {
        if let Some(exclude) = spec.strip_prefix(":(exclude)") {
            if submodule_path_matches_pathspec(path, exclude) {
                matched = false;
            }
        } else if *spec == "." || submodule_path_matches_pathspec(path, spec) {
            matched = true;
        }
    }
    matched
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
    if !submodule_root.join(".git").is_dir() {
        return Ok(());
    }
    relocate_submodule_git_dir(
        git_dir,
        &submodule_root,
        &submodule.name,
        &submodule.path,
        quiet,
    )
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
    // Bare `foreach` (no command): git's argv[0] is null, so it runs nothing.
    if options.args.is_empty() {
        return Ok(());
    }
    // git's `runcommand_in_submodule_cb` + `run-command`'s `use_shell`:
    //  - argc == 1: a single command string is run through the shell, with the
    //    per-submodule env vars ($name/$sm_path/$displaypath/$sha1/$toplevel)
    //    exported and `path=<sq-quoted>;` prefixed. The shell interprets quotes.
    //  - argc > 1: the args run AS-IS (no re-evaluation). `prepare_shell_cmd`
    //    only wraps them in `sh -c '<argv0> "$@"' ...` when argv0 contains a
    //    shell metacharacter; a plain argv0 (e.g. `echo`) execs directly, so
    //    quotes inside later args stay literal.
    let mut command = ProcessCommand::new("sh");
    if options.args.len() == 1 {
        let toplevel =
            fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
        command
            .arg("-c")
            .arg(format!(
                "path={}; {}",
                shell_single_quote(&submodule.path),
                options.args[0]
            ))
            .env("name", &submodule.name)
            .env("sm_path", &submodule.path)
            .env("displaypath", &display_path)
            .env("sha1", &sha1)
            .env("toplevel", &toplevel);
    } else if shell_needs_quoting(&options.args[0]) {
        command
            .arg("-c")
            .arg(format!("{} \"$@\"", options.args[0]))
            .arg(&options.args[0]);
        command.args(&options.args[1..]);
    } else {
        // Plain argv0: exec it directly, args verbatim (no shell).
        command = ProcessCommand::new(&options.args[0]);
        command.args(&options.args[1..]);
    }
    // Inherit stdin/stdout/stderr so the child sees the parent's stdin (git's
    // `read y` test pipes `yes` into foreach) and its stdout/stderr go straight
    // to the parent's (already-redirected) descriptors. Flush our own buffered
    // "Entering" line first so ordering is preserved.
    io::stdout().flush()?;
    let status = command
        .current_dir(&submodule_root)
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if status.success() {
        return Ok(());
    }
    eprintln!("fatal: run_command returned non-zero status for {display_path}");
    eprintln!(".");
    Err(GitError::Exit(128))
}

/// `run-command.c prepare_shell_cmd`'s metacharacter test: argv[0] needs a shell
/// only when it contains one of the shell-special bytes.
fn shell_needs_quoting(arg0: &str) -> bool {
    arg0.bytes()
        .any(|b| b"|&;<>()$`\\\"' \t\n*?[#~=%".contains(&b))
}

/// `sq_quote_buf`: single-quote a string for safe inclusion in a shell command.
fn shell_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Port of `generate_submodule_summary` (git `builtin/submodule--helper.c`).
/// Emits one `* <path> <src>...<dst> (<n>):` block per gitlink change, with the
/// `--pretty`-style log lines (`> `/`< ` for forward/backward, or the
/// type-change blob/submodule header). The submodule repo provides the commit
/// objects; a submodule that isn't checked out (no readable repo) is skipped for
/// modifications, matching git's `is_nonbare_repository_dir` gate, but
/// additions/deletions/type-changes still print their header.
fn generate_submodule_summary(
    cwd: &Path,
    worktree_root: &Path,
    _index: &Option<Index>,
    entry: &SubmoduleSummaryEntry,
    cached: bool,
    summary_limit: Option<isize>,
) -> Result<()> {
    let submodule_root = worktree_root.join(&entry.sm_path);
    let sub_repo = submodule_head(&submodule_root)
        .ok()
        .map(|(git_dir, _)| git_dir);

    // git's `generate_submodule_summary`: when not --cached and the dst oid is
    // null but the path is a gitlink, fill it from the submodule's HEAD; when the
    // dst is a blob/regular file, hash the worktree file. We only need the
    // gitlink-from-HEAD branch for the summary-without-cached forward case.
    let mut oid_dst = entry.oid_dst;
    if !cached && entry.oid_dst.is_null() && entry.mod_dst == 0o160000 {
        if let Some(git_dir) = &sub_repo {
            let format = repository_object_format(git_dir)?;
            if let Ok(head_oid) = resolve_revision(git_dir, format, "HEAD") {
                oid_dst = head_oid;
            }
        }
    }
    let oid_src = entry.oid_src;

    // Skip a plain modification whose submodule isn't checked out (git's
    // is_nonbare_repository_dir gate in prepare_submodule_summary). Adds /
    // deletes / type-changes still print.
    if (entry.status == 'M') && sub_repo.is_none() {
        return Ok(());
    }

    let src_is_gitlink = entry.mod_src == 0o160000;
    let dst_is_gitlink = entry.mod_dst == 0o160000;

    // Abbreviations: git runs `rev-parse --short <oid>^0` inside the submodule;
    // a missing commit falls back to the 7-char prefix of the full oid. We use
    // the 7-char prefix uniformly (the test submodules are small, so the unique
    // abbrev is 7), and track "missing" to suppress the commit count.
    let (src_abbrev, missing_src) = summary_abbrev(
        sub_repo.as_deref(),
        &oid_src,
        src_is_gitlink,
        entry.status == 'D',
    );
    let (dst_abbrev, missing_dst) =
        summary_abbrev(sub_repo.as_deref(), &oid_dst, dst_is_gitlink, false);

    let display_path = display_submodule_path(cwd, worktree_root, &entry.sm_path)?;

    // Commit count via the submodule repo (git: rev-list --first-parent --count).
    let mut total_commits: Option<usize> = None;
    let mut errmsg: Option<String> = None;
    if !missing_src && !missing_dst {
        if let Some(git_dir) = &sub_repo {
            let format = repository_object_format(git_dir)?;
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            if src_is_gitlink && dst_is_gitlink {
                total_commits = Some(summary_symmetric_count(&db, format, &oid_src, &oid_dst)?);
            } else {
                // Single-sided (add/delete/type-change): count from the present
                // side back to root.
                let side = if src_is_gitlink { &oid_src } else { &oid_dst };
                total_commits = Some(summary_count_to_root(&db, format, side)?);
            }
        }
    } else if dst_is_gitlink {
        // Missing-commit warning, only when the dst is still a submodule.
        let msg = if missing_src && missing_dst {
            format!(
                "  Warn: {display_path} doesn't contain commits {} and {}\n",
                oid_src.to_hex(),
                oid_dst.to_hex()
            )
        } else {
            let missing = if missing_src { &oid_src } else { &oid_dst };
            format!(
                "  Warn: {display_path} doesn't contain commit {}\n",
                missing.to_hex()
            )
        };
        errmsg = Some(msg);
    }

    // Header line.
    if entry.status == 'T' {
        if dst_is_gitlink {
            print!("* {display_path} {src_abbrev}(blob)->{dst_abbrev}(submodule)");
        } else {
            print!("* {display_path} {src_abbrev}(submodule)->{dst_abbrev}(blob)");
        }
    } else {
        print!("* {display_path} {src_abbrev}...{dst_abbrev}");
    }
    match total_commits {
        Some(n) => println!(" ({n}):"),
        None => println!(":"),
    }

    // Body.
    if let Some(msg) = errmsg {
        print!("{msg}");
    } else if total_commits.is_some_and(|n| n > 0) {
        if let Some(git_dir) = &sub_repo {
            let format = repository_object_format(git_dir)?;
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let limit = summary_limit.and_then(|limit| usize::try_from(limit).ok());
            if src_is_gitlink && dst_is_gitlink {
                // log --pretty='  %m %s' --first-parent <src>...<dst>: the `%m`
                // marker is `>` for commits reachable only from dst (forward),
                // `<` for commits reachable only from src (backward). git lists
                // the dst-only (`>`) commits first, then the src-only (`<`).
                let marked = summary_symmetric_log(&db, format, &oid_src, &oid_dst)?;
                let take = limit.unwrap_or(marked.len());
                for (marker, commit) in marked.iter().take(take) {
                    println!("  {marker} {}", commit_subject(&commit.message));
                }
            } else if dst_is_gitlink {
                // log --pretty='  > %s' -1 <dst>
                let commit = summary_tip_commit(&db, format, &oid_dst)?;
                if let Some(commit) = commit {
                    println!("  > {}", commit_subject(&commit.message));
                }
            } else {
                // log --pretty='  < %s' -1 <src>
                let commit = summary_tip_commit(&db, format, &oid_src)?;
                if let Some(commit) = commit {
                    println!("  < {}", commit_subject(&commit.message));
                }
            }
        }
    }
    println!();
    Ok(())
}

/// Abbreviate a summary-side oid the way `verify_submodule_committish` does:
/// if the commit is present in the submodule repo, use its short hash (7 chars
/// here); if it's null/absent, fall back to the 7-char prefix and mark missing.
/// `is_delete_src` suppresses the existence check for a deletion's src (git skips
/// `verify_submodule_committish` for status 'D').
fn summary_abbrev(
    sub_git_dir: Option<&Path>,
    oid: &ObjectId,
    is_gitlink: bool,
    is_delete_src: bool,
) -> (String, bool) {
    let hex7 = format_log_abbrev_oid(oid);
    if !is_gitlink {
        // Non-gitlink side: always the plain prefix, never "missing".
        return (hex7, false);
    }
    if oid.is_null() {
        return (hex7, false);
    }
    if is_delete_src {
        return (hex7, false);
    }
    // Present in the submodule repo?
    if let Some(git_dir) = sub_git_dir {
        if let Ok(format) = repository_object_format(git_dir) {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            if db.read_object(oid).is_ok() {
                return (hex7, false);
            }
        }
    }
    (hex7, true)
}

/// `rev-list --first-parent --count A...B` for two commits in the submodule:
/// the number of commits reachable from exactly one side (the symmetric
/// difference along first-parent chains).
fn summary_symmetric_count(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    a: &ObjectId,
    b: &ObjectId,
) -> Result<usize> {
    let forward = summary_first_parent_only(db, format, b, a)?;
    let backward = summary_first_parent_only(db, format, a, b)?;
    Ok(forward.len() + backward.len())
}

/// Single-sided count to the root (`rev-list --first-parent --count <oid>`).
fn summary_count_to_root(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<usize> {
    Ok(summary_first_parent_chain(db, format, oid)?.len())
}

/// Walk the first-parent chain from `tip` and return the commits not reachable
/// (along the same chain) from `base`. Mirrors `rev-list --first-parent base..tip`.
fn summary_first_parent_only(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tip: &ObjectId,
    base: &ObjectId,
) -> Result<Vec<(ObjectId, Commit)>> {
    let base_chain: HashSet<ObjectId> = summary_first_parent_chain(db, format, base)?
        .into_iter()
        .map(|(oid, _)| oid)
        .collect();
    let mut out = Vec::new();
    for (oid, commit) in summary_first_parent_chain(db, format, tip)? {
        if base_chain.contains(&oid) {
            break;
        }
        out.push((oid, commit));
    }
    Ok(out)
}

/// The full first-parent chain from `oid` back to the root.
fn summary_first_parent_chain(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<(ObjectId, Commit)>> {
    let mut chain = Vec::new();
    let mut current = Some(*oid);
    let mut seen = HashSet::new();
    while let Some(cur) = current {
        if !seen.insert(cur) {
            break;
        }
        let object = match db.read_object(&cur) {
            Ok(object) => object,
            Err(_) => break,
        };
        if object.object_type != ObjectType::Commit {
            break;
        }
        let commit = Commit::parse(format, &object.body)?;
        current = commit.parents.first().copied();
        chain.push((cur, commit));
    }
    Ok(chain)
}

/// `log --pretty='  %m %s' --first-parent <src>...<dst>` for a two-gitlink
/// modification: the symmetric difference along first-parent chains, with the
/// dst-only commits marked `>` (forward) listed first, then the src-only commits
/// marked `<` (backward). Both sub-lists are newest-first, matching git's
/// reverse-chronological log over the `A...B` range.
fn summary_symmetric_log(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    src: &ObjectId,
    dst: &ObjectId,
) -> Result<Vec<(char, Commit)>> {
    let mut out = Vec::new();
    for (_, commit) in summary_first_parent_only(db, format, dst, src)? {
        out.push(('>', commit));
    }
    for (_, commit) in summary_first_parent_only(db, format, src, dst)? {
        out.push(('<', commit));
    }
    Ok(out)
}

/// `log -1 <oid>`: the single commit at `oid`, if present.
fn summary_tip_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<Commit>> {
    let Ok(object) = db.read_object(oid) else {
        return Ok(None);
    };
    if object.object_type != ObjectType::Commit {
        return Ok(None);
    }
    Ok(Some(Commit::parse(format, &object.body)?))
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

pub(crate) fn read_submodule_configs(worktree_root: &Path) -> Result<Vec<SubmoduleConfigEntry>> {
    // PILOT (sley-submodule): the hand-rolled section walk that used to live
    // here is now the typed `submodule-config.c` port. We map the typed
    // `Submodule` back onto the CLI's `SubmoduleConfigEntry` so the dozen-odd
    // call sites are untouched; only the parse is centralized. The new parser
    // is `overwrite == 0` (first-value-wins) like git, whereas the old walk
    // used `.rev().find()` (last-value-wins). git's own `.gitmodules` parse is
    // first-wins, so this is a parity fix, not a behavior break.
    //
    // TODO(submodule): migrate the other 13 `.gitmodules` walk sites
    // (set-url / set-branch / sync / add via `submodule_name_for_exact_path`,
    // and the scattered reads in sley-cli/{branch,workspace,remote_cmds}.rs,
    // sley-worktree, sley-remote/clone.rs) onto `SubmoduleConfigSet`.
    let path = worktree_root.join(".gitmodules");
    let Ok(config) = GitConfig::read(path) else {
        return Ok(Vec::new());
    };
    let set = sley_submodule::SubmoduleConfigSet::parse(&config);
    // git die()s "invalid value for 'submodule.<name>.update'" the moment any
    // command parses a `.gitmodules` with a bad update value (an unrecognized
    // mode or a forbidden `!command`). The typed parser surfaces that as an
    // `InvalidUpdate` warning; promote the FIRST one to git's fatal here so
    // status / init / update all reject it identically.
    if let Some(sley_submodule::ParseWarning::InvalidUpdate { name }) = set
        .warnings
        .iter()
        .find(|w| matches!(w, sley_submodule::ParseWarning::InvalidUpdate { .. }))
    {
        eprintln!("fatal: invalid value for 'submodule.{name}.update'");
        return Err(GitError::Exit(128));
    }
    // git's `.gitmodules` parser warns (and drops the value) for a path/url that
    // could be mistaken for a command-line option (`warn_command_line_option`).
    // The typed parser already dropped it; surface the warning so a recursive
    // clone/update reports "ignoring '<var>' ..." rather than silently failing.
    for warning in &set.warnings {
        if let sley_submodule::ParseWarning::CommandLineOption { var, value } = warning {
            eprintln!(
                "warning: ignoring '{var}' which may be interpreted as a command-line option: {value}"
            );
        }
    }
    let mut submodules = Vec::new();
    for submodule in set.iter() {
        // A submodule with no `path` is not addressable; the old walk skipped
        // those too (`if let Some(path) = path`).
        let Some(path) = submodule.path.clone() else {
            continue;
        };
        submodules.push(SubmoduleConfigEntry {
            name: submodule.name.clone(),
            path,
            url: submodule.url.clone(),
            update: submodule_update_to_raw(&submodule.update_strategy),
        });
    }
    Ok(submodules)
}

pub(crate) fn index_gitlink_submodule_configs(
    git_dir: &Path,
    format: ObjectFormat,
    configured: &[SubmoduleConfigEntry],
) -> Result<Vec<SubmoduleConfigEntry>> {
    let Some(index) = read_repository_index(git_dir, format)? else {
        return Ok(Vec::new());
    };
    let configured_paths = configured
        .iter()
        .map(|submodule| submodule.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut submodules = Vec::new();
    for entry in index.entries {
        if !sley_index::is_gitlink(entry.mode) {
            continue;
        }
        let Ok(path) = String::from_utf8(entry.path.to_vec()) else {
            continue;
        };
        if configured_paths.contains(path.as_str()) {
            continue;
        }
        submodules.push(SubmoduleConfigEntry {
            name: path.clone(),
            path,
            url: None,
            update: None,
        });
    }
    Ok(submodules)
}

pub(crate) fn ensure_populated_gitlinks_readable(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let Some(index) = read_repository_index(git_dir, format)? else {
        return Ok(());
    };
    for entry in index.entries {
        if !sley_index::is_gitlink(entry.mode) {
            continue;
        }
        let Ok(path) = String::from_utf8(entry.path.to_vec()) else {
            continue;
        };
        let sub_root = worktree_root.join(path);
        if sley_diff_merge::gitlink_git_dir(&sub_root).is_some() {
            ensure_gitlink_head_readable(&sub_root, format)?;
        }
    }
    Ok(())
}

fn ensure_gitlink_head_readable(sub_root: &Path, format: ObjectFormat) -> Result<()> {
    let Some(git_dir) = sley_diff_merge::gitlink_git_dir(sub_root) else {
        return Ok(());
    };
    let store = FileRefStore::new(&git_dir, format);
    let mut target = store
        .read_ref("HEAD")?
        .ok_or_else(|| broken_submodule_repository(&git_dir))?;
    for _ in 0..10 {
        match target {
            RefTarget::Direct(oid) => {
                let db = FileObjectDatabase::from_git_dir(&git_dir, format);
                let object = db
                    .read_object(&oid)
                    .map_err(|_| broken_submodule_repository(&git_dir))?;
                if object.object_type == ObjectType::Commit {
                    return Ok(());
                }
                return Err(broken_submodule_repository(&git_dir));
            }
            RefTarget::Symbolic(name) => {
                target = store
                    .read_ref(&name)?
                    .ok_or_else(|| broken_submodule_repository(&git_dir))?;
            }
        }
    }
    Err(broken_submodule_repository(&git_dir))
}

fn broken_submodule_repository(git_dir: &Path) -> GitError {
    eprintln!("fatal: not a git repository: {}", git_dir.display());
    GitError::Exit(128)
}

/// Re-stringify a typed update strategy back to the raw `.gitmodules` value the
/// init path copies into `.git/config`. `Unspecified` (never set, or an
/// invalid value the typed parser rejected) maps to `None`, matching the old
/// behavior where only a present, recognized `update =` line was copied.
fn submodule_update_to_raw(strategy: &sley_submodule::UpdateStrategy) -> Option<String> {
    use sley_submodule::UpdateType;
    match strategy.kind {
        UpdateType::Unspecified => None,
        UpdateType::Command => strategy
            .command
            .as_ref()
            .map(|command| format!("!{command}")),
        other => sley_submodule::update_type_to_string(other).map(str::to_string),
    }
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

pub(crate) fn submodule_path_matches_pathspec(path: &str, pathspec: &str) -> bool {
    pathspec.is_empty()
        || path == pathspec
        || path
            .strip_prefix(pathspec)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(crate) fn normalize_submodule_pathspec(cwd: &Path, worktree_root: &Path, path: &str) -> String {
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

/// git's `get_submodule_displaypath`: when a `super_prefix` is carried (a
/// recursive child), the displaypath is `super_prefix + path` so nested
/// submodule messages are anchored at the recursion root (e.g. `middle/bottom`
/// rather than just `bottom`). At the top level the prefix is empty and we fall
/// back to the cwd-relative form.
fn submodule_displaypath(
    cwd: &Path,
    worktree_root: &Path,
    path: &str,
    super_prefix: &str,
) -> Result<String> {
    if super_prefix.is_empty() {
        display_submodule_path(cwd, worktree_root, path)
    } else {
        Ok(format!("{super_prefix}{path}"))
    }
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
    // git's `compute_rev_name` ends with `git describe --all --always <oid>`,
    // whose `--always` fallback prints the abbreviated commit hash for an oid
    // no ref tip resolves to (a detached commit, e.g. an ancestor of the
    // submodule's branch). Reproduce that bare-hash form so status never shows
    // an empty suffix when the oid is a real commit.
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    if db.read_object(oid).is_ok() {
        return Ok(format!(" ({})", format_log_abbrev_oid(oid)));
    }
    Ok(String::new())
}

fn display_submodule_ref(name: &str) -> String {
    if let Some(tag) = name.strip_prefix("refs/tags/") {
        return tag.to_string();
    }
    name.strip_prefix("refs/").unwrap_or(name).to_string()
}

/// `git submodule--helper <subcommand>`: the plumbing entry point git's porcelain
/// shells out to. Only the subcommands the upstream test-suite exercises directly
/// are wired here; the porcelain verbs go through `cmd_submodule`.
pub(crate) fn cmd_submodule_helper(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("get-default-remote") => submodule_helper_get_default_remote(&args[1..]),
        Some(other) => {
            eprintln!("fatal: '{other}' is not a valid submodule--helper subcommand");
            Err(GitError::Exit(1))
        }
        None => {
            eprintln!("usage: git submodule--helper");
            Err(GitError::Exit(129))
        }
    }
}

/// Port of `module_get_default_remote` (`builtin/submodule--helper.c`): resolve
/// the default remote NAME of the submodule populated at `<path>`. The lookup is
/// url-first — a remote whose configured url matches the submodule's (relative-
/// resolved) `.gitmodules` url wins — then falls back to `repo_default_remote`
/// (the HEAD branch's remote, else the sole remote, else "origin").
fn submodule_helper_get_default_remote(args: &[String]) -> Result<()> {
    const USAGE: &str = "usage: git submodule--helper get-default-remote <path>";
    let mut paths = Vec::new();
    let mut end_opts = false;
    for arg in args {
        if !end_opts && arg == "--" {
            end_opts = true;
            continue;
        }
        if !end_opts && arg == "-h" {
            println!("{USAGE}");
            return Ok(());
        }
        if !end_opts && arg.len() > 1 && arg.starts_with('-') {
            eprintln!("{USAGE}");
            return Err(GitError::Exit(129));
        }
        paths.push(arg.clone());
    }
    if paths.len() != 1 {
        eprintln!("{USAGE}");
        return Err(GitError::Exit(129));
    }
    let path_arg = &paths[0];

    let cwd = env::current_dir()?;
    let super_git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&super_git_dir)?;

    // git prepends the `prefix` (cwd relative to the worktree root) to a
    // relative path; `cwd.join(path_arg)` reaches the same worktree location.
    let abs_path = if Path::new(path_arg).is_absolute() {
        PathBuf::from(path_arg)
    } else {
        cwd.join(path_arg)
    };
    let abs_path = normalize_lexical_path(&abs_path);

    // `repo_submodule_init`: try the populated worktree's `.git` first, then the
    // superproject's `modules/<name>` git dir for a configured-but-unpopulated
    // submodule. Either failure is git's "could not get a repository handle".
    let sub_git_dir = resolve_submodule_repo_gitdir(&abs_path)
        .or_else(|| modules_gitdir_for_worktree_path(&worktree_root, &super_git_dir, &abs_path));
    let Some(sub_git_dir) = sub_git_dir else {
        eprintln!("fatal: could not get a repository handle for submodule '{path_arg}'");
        return Err(GitError::Exit(128));
    };
    let sub_format = repository_object_format(&sub_git_dir)?;
    let sub_config = read_repo_config(&sub_git_dir)?;
    let remote_name = submodule_default_remote_name(
        &worktree_root,
        &super_git_dir,
        &abs_path,
        &sub_git_dir,
        sub_format,
        &sub_config,
    )?;
    println!("{remote_name}");
    Ok(())
}

/// Core of `get_default_remote_submodule`: the default remote NAME for the
/// already-resolved submodule (`abs_path` worktree, `sub_git_dir` git dir).
/// url-match first (a remote whose url equals the submodule's relative-resolved
/// `.gitmodules` url), then `repo_default_remote`.
fn submodule_default_remote_name(
    worktree_root: &Path,
    super_git_dir: &Path,
    abs_path: &Path,
    sub_git_dir: &Path,
    sub_format: ObjectFormat,
    sub_config: &GitConfig,
) -> Result<String> {
    let mut remote_name = None;
    if let Some(url) = configured_submodule_url(worktree_root, super_git_dir, abs_path)? {
        remote_name = remote_name_for_url(sub_config, &url);
    }
    Ok(remote_name.unwrap_or_else(|| repo_default_remote(sub_config, sub_git_dir, sub_format)))
}

/// Resolve the git dir for a populated submodule worktree at `worktree`: a `.git`
/// subdir, or a `gitdir:`-pointer file. Returns `None` when neither is present
/// (so the caller can fall back / die without walking up to the superproject).
fn resolve_submodule_repo_gitdir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    if dot_git.is_dir() {
        return fs::canonicalize(&dot_git).ok();
    }
    if dot_git.is_file()
        && let Ok(Some(target)) = read_gitdir_file(&dot_git)
        && target.is_dir()
    {
        return fs::canonicalize(&target).ok();
    }
    None
}

/// `repo_submodule_init`'s fallback: a configured submodule whose worktree is not
/// populated still has its git dir at `<super>/modules/<name>`. Look the name up
/// from the superproject `.gitmodules` keyed by the worktree-relative path.
fn modules_gitdir_for_worktree_path(
    worktree_root: &Path,
    super_git_dir: &Path,
    abs_path: &Path,
) -> Option<PathBuf> {
    let rel = abs_path.strip_prefix(worktree_root).ok()?;
    let rel = rel.to_str()?;
    let submodules = read_submodule_configs(worktree_root).ok()?;
    let name = &submodules.iter().find(|sub| sub.path == rel)?.name;
    let gitdir = super_git_dir.join("modules").join(name);
    gitdir.is_dir().then_some(gitdir)
}

/// The submodule's `.gitmodules` url for the worktree at `abs_path`, resolved to
/// the concrete form when relative (matching what `submodule add` records as the
/// sub-repo's `origin.url`). `None` when the path is not a configured submodule.
fn configured_submodule_url(
    worktree_root: &Path,
    super_git_dir: &Path,
    abs_path: &Path,
) -> Result<Option<String>> {
    let Ok(rel) = abs_path.strip_prefix(worktree_root) else {
        return Ok(None);
    };
    let Some(rel) = rel.to_str() else {
        return Ok(None);
    };
    let submodules = read_submodule_configs(worktree_root)?;
    let Some(url) = submodules
        .iter()
        .find(|sub| sub.path == rel)
        .and_then(|sub| sub.url.clone())
    else {
        return Ok(None);
    };
    if url.starts_with("../") || url.starts_with("./") {
        let super_config = read_repo_config(super_git_dir)?;
        Ok(Some(resolve_submodule_sync_url(
            worktree_root,
            &super_config,
            &url,
        )))
    } else {
        Ok(Some(url))
    }
}

/// `repo_remote_from_url`: the name of the first remote with a url exactly equal
/// to `url` (remotes in config first-appearance order).
fn remote_name_for_url(config: &GitConfig, url: &str) -> Option<String> {
    for name in distinct_remote_names(config) {
        if config
            .get_all("remote", Some(&name), "url")
            .into_iter()
            .flatten()
            .any(|configured| configured == url)
        {
            return Some(name);
        }
    }
    None
}

/// `repo_default_remote`: the HEAD branch's `branch.<name>.remote`, else the sole
/// remote's name when exactly one is configured, else "origin".
fn repo_default_remote(config: &GitConfig, git_dir: &Path, format: ObjectFormat) -> String {
    let store = FileRefStore::new(git_dir, format);
    if let Ok(Some(RefTarget::Symbolic(target))) = store.read_ref("HEAD")
        && let Some(branch) = target.strip_prefix("refs/heads/")
        && let Some(remote) = config.get("branch", Some(branch), "remote")
    {
        return remote.to_string();
    }
    let remotes = distinct_remote_names(config);
    if remotes.len() == 1 {
        return remotes[0].clone();
    }
    "origin".to_string()
}

/// Remote names in first-appearance order across the config sections.
fn distinct_remote_names(config: &GitConfig) -> Vec<String> {
    let mut names = Vec::new();
    for section in &config.sections {
        if section.name == "remote"
            && let Some(sub) = &section.subsection
            && !names.iter().any(|existing| existing == sub)
        {
            names.push(sub.clone());
        }
    }
    names
}
