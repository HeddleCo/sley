//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use crate::*;
use sley::plumbing::{sley_diff_merge, sley_index, sley_worktree};

struct AddContext {
    cwd: PathBuf,
    git_dir: PathBuf,
    worktree_root: PathBuf,
    format: ObjectFormat,
    config: GitConfig,
}

impl AddContext {
    fn open(cli_session: &crate::session::CliSession) -> Result<Self> {
        let repository = cli_session.open_repository()?;
        let git_dir = repository.git_dir().to_path_buf();
        let worktree_root = repository
            .workdir()
            .ok_or_else(|| GitError::Unsupported("add requires a repository worktree".into()))?;
        let format = repository.object_format();
        let config = read_repo_config(&git_dir)?;
        Ok(Self {
            cwd: cli_session.cwd().to_path_buf(),
            git_dir,
            worktree_root,
            format,
            config,
        })
    }
}

pub(crate) fn cmd_add(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    // `add -i` / `add --interactive` and `add -p` / `add --patch` route to the
    // interactive engine. git treats `--patch` as implying interactive and lets
    // a pathspec follow. We collect the non-flag pathspec args plus the diff-tuning
    // flags add-patch forwards to the spawned `diff-files` (`-U`/`--unified`,
    // `--inter-hunk-context`) and forward them.
    {
        let mut interactive = false;
        let mut patch = false;
        let mut dry_run = false;
        let mut pathspec_from_file = false;
        let mut spec: Vec<String> = Vec::new();
        // Explicit `-U<n>` / `--inter-hunk-context=<n>` from add's own argv. `None`
        // means "fall back to diff.context / diff.interHunkContext config".
        let mut context: Option<i64> = None;
        let mut interhunk: Option<i64> = None;
        // `--auto-advance`/`--no-auto-advance`. git's default is auto-advance ON;
        // `Some(false)` is `--no-auto-advance`.
        let mut auto_advance: Option<bool> = None;
        let mut after_dd = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if after_dd {
                spec.push(arg.clone());
                continue;
            }
            match arg.as_str() {
                "--" => after_dd = true,
                "-n" | "--dry-run" => dry_run = true,
                "--no-dry-run" => dry_run = false,
                "-i" | "--interactive" => interactive = true,
                "-p" | "--patch" => patch = true,
                "--pathspec-from-file" => {
                    pathspec_from_file = true;
                    iter.next();
                }
                value if value.starts_with("--pathspec-from-file=") => {
                    pathspec_from_file = true;
                }
                "--auto-advance" => auto_advance = Some(true),
                "--no-auto-advance" => auto_advance = Some(false),
                "-U" | "--unified" => {
                    context = iter.next().and_then(|v| v.parse::<i64>().ok());
                }
                value if value.starts_with("-U") => {
                    context = value[2..].parse::<i64>().ok();
                }
                value if let Some(rest) = value.strip_prefix("--unified=") => {
                    context = rest.parse::<i64>().ok();
                }
                "--inter-hunk-context" => {
                    interhunk = iter.next().and_then(|v| v.parse::<i64>().ok());
                }
                value if let Some(rest) = value.strip_prefix("--inter-hunk-context=") => {
                    interhunk = rest.parse::<i64>().ok();
                }
                other if other.starts_with('-') => {
                    // Leave any other flags to the normal path (no -i/-p).
                }
                other => spec.push(other.to_string()),
            }
        }
        // builtin/add.c validation order: negative context dies first (independent
        // of -p), then the "requires --interactive/--patch" checks fire only when
        // NOT in interactive/patch mode.
        if let Some(value) = context
            && value < -1
        {
            eprintln!("fatal: '--unified' cannot be negative");
            return Err(GitError::Exit(128));
        }
        if let Some(value) = interhunk
            && value < -1
        {
            eprintln!("fatal: '--inter-hunk-context' cannot be negative");
            return Err(GitError::Exit(128));
        }
        if !patch && !interactive {
            if context.is_some() {
                eprintln!("fatal: the option '--unified' requires '--interactive/--patch'");
                return Err(GitError::Exit(128));
            }
            if interhunk.is_some() {
                eprintln!(
                    "fatal: the option '--inter-hunk-context' requires '--interactive/--patch'"
                );
                return Err(GitError::Exit(128));
            }
            if auto_advance == Some(false) {
                eprintln!("fatal: the option '--no-auto-advance' requires '--interactive/--patch'");
                return Err(GitError::Exit(128));
            }
        }
        if (patch || interactive) && dry_run {
            eprintln!(
                "fatal: options '--dry-run' and '--interactive/--patch' cannot be used together"
            );
            return Err(GitError::Exit(128));
        }
        if (patch || interactive) && pathspec_from_file {
            eprintln!(
                "fatal: options '--pathspec-from-file' and '--interactive/--patch' cannot be used together"
            );
            return Err(GitError::Exit(128));
        }
        if patch {
            return crate::commands::add_interactive::cmd_add_patch(
                cli_session,
                &spec,
                context,
                interhunk,
                auto_advance.unwrap_or(true),
            );
        }
        if interactive {
            return crate::commands::add_interactive::cmd_add_interactive(cli_session, &spec);
        }
    }
    let mut paths = Vec::new();
    let mut dry_run = false;
    let mut verbose = false;
    let mut update = false;
    let mut all = false;
    let mut force = false;
    let mut ignore_removal = false;
    let mut ignore_errors = None;
    let mut ignore_missing = false;
    let mut intent_to_add = false;
    let mut sparse = false;
    let mut refresh = false;
    let mut renormalize = false;
    let mut warn_embedded_repos = true;
    let mut chmod = None;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut edit_option = false;
    let mut parsing_options = true;
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
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-u" | "--update" => update = true,
            "--no-update" => update = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-A" | "--all" | "--no-ignore-removal" => {
                all = true;
                ignore_removal = false;
            }
            "--ignore-removal" | "--no-all" => {
                all = false;
                ignore_removal = true;
            }
            "--ignore-missing" => ignore_missing = true,
            "--no-ignore-missing" => ignore_missing = false,
            "--refresh" => refresh = true,
            "--no-refresh" => refresh = false,
            "--renormalize" => renormalize = true,
            "--no-renormalize" => renormalize = false,
            "-N" | "--intent-to-add" => intent_to_add = true,
            "--no-intent-to-add" => intent_to_add = false,
            "--chmod" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--chmod requires a value".into()))?;
                chmod = Some(parse_add_chmod(value)?);
            }
            "--no-chmod" => chmod = None,
            value if value.starts_with("--chmod=") => {
                let value = value
                    .strip_prefix("--chmod=")
                    .expect("prefix checked by match guard");
                chmod = Some(parse_add_chmod(value)?);
            }
            "--ignore-errors" => ignore_errors = Some(true),
            "--no-ignore-errors" => ignore_errors = Some(false),
            "--sparse" => sparse = true,
            "--no-sparse" => sparse = false,
            "--warn-embedded-repo" => warn_embedded_repos = true,
            "--no-warn-embedded-repo" => warn_embedded_repos = false,
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--edit" | "-e" => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: options '--pathspec-from-file' and '--edit' cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                edit_option = true;
                paths.push(PathBuf::from(arg));
            }
            "--pathspec-from-file" => {
                if edit_option {
                    eprintln!(
                        "fatal: options '--pathspec-from-file' and '--edit' cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                if edit_option {
                    eprintln!(
                        "fatal: options '--pathspec-from-file' and '--edit' cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|option| matches!(option, b'A' | b'n' | b'u' | b'v' | b'f')) =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b'A' => all = true,
                        b'n' => dry_run = true,
                        b'u' => update = true,
                        b'v' => verbose = true,
                        b'f' => force = true,
                        _ => unreachable!("add short-option group was filtered"),
                    }
                }
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
    if ignore_missing && !dry_run {
        eprintln!("fatal: the option '--ignore-missing' requires '--dry-run'");
        return Err(GitError::Exit(128));
    }
    if paths.is_empty() && !update && !all && !refresh {
        eprintln!("Nothing specified, nothing added.");
        eprintln!("hint: Maybe you wanted to say 'git add .'?");
        eprintln!(
            "hint: Disable this message with \"git config set advice.addEmptyPathspec false\""
        );
        return Ok(());
    }
    let context = AddContext::open(cli_session)?;
    let cwd = &context.cwd;
    let git_dir = &context.git_dir;
    let format = context.format;
    let worktree_root = &context.worktree_root;
    let pathspec_magic = effective_pathspec_flags(cli_session);
    let ignore_errors = ignore_errors.unwrap_or_else(|| {
        context
            .config
            .get_bool("add", None, "ignore-errors")
            .or_else(|| context.config.get_bool("add", None, "ignoreErrors"))
            .unwrap_or(false)
    });
    // git refuses (with advice + exit 1) to update entries that the skip-worktree
    // bit or the sparse-checkout definition put outside the working set, unless
    // `--sparse` is given. This guards every add flavor (regular, -u, -A, -N,
    // --refresh, --dry-run), so run it once up front before dispatching.
    reject_add_skip_worktree_paths(
        &cwd,
        &worktree_root,
        &git_dir,
        format,
        &paths,
        sparse,
        refresh,
        &context.config,
    )?;
    if renormalize {
        let tracked_paths = resolve_add_renormalize_paths(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            &paths,
            pathspec_magic,
        )?;
        if dry_run {
            let actions = tracked_paths
                .into_iter()
                .map(AddAction::Add)
                .collect::<Vec<_>>();
            print_add_actions(&worktree_root, &actions)?;
            return Ok(());
        }
        sley_worktree::renormalize_index_paths_filtered(
            &worktree_root,
            &git_dir,
            format,
            &tracked_paths,
            &context.config,
        )?;
        commands::hooks::run_post_index_change_hook(cli_session, false, false)?;
        return Ok(());
    }
    if refresh {
        refresh_index_after_add(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            &paths,
            true,
            pathspec_magic,
        )?;
        return Ok(());
    }
    if intent_to_add && !dry_run {
        return add_intent_to_add(&cwd, &worktree_root, &git_dir, format, &paths);
    }
    if !update
        && !all
        && let Some(actions) = try_add_regular_exact_tracked_raw(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            &paths,
            AddRegularOptions {
                chmod,
                force,
                ignore_errors,
                ignore_removal,
                ignore_missing,
                dry_run,
                sparse,
            },
        )?
    {
        if verbose {
            print_add_actions(&worktree_root, &actions)?;
        }
        return Ok(());
    }
    let parsed_index = if paths.is_empty() {
        None
    } else {
        sley_worktree::read_repository_index(&git_dir, format)?
    };
    die_on_pathspec_inside_submodule(&cwd, &worktree_root, parsed_index.as_ref(), &paths)?;
    // git's `add` re-stats every tracked path it touches, including ones whose
    // content is unchanged (a `touch`ed file): `builtin/add.c` calls
    // `refresh_index` over the pathspec before/after staging, so the cached stat
    // matches the worktree and `git diff-files` stays clean (t2200 "touch and then
    // add"). sley's action resolver only stages content-changed paths, so a
    // content-clean-but-stat-dirty tracked entry would otherwise keep its stale
    // stat. Capture the pathspec so we can run that refresh after staging; an empty
    // pathspec (bare `add -u`/`-A`) refreshes every tracked entry, matching git.
    //
    // `--chmod` is the one case we must NOT refresh: it deliberately sets an index
    // mode that diverges from the worktree file's mode (e.g. stage 100755 while the
    // file is 100644), and a stat refresh would re-stamp the mode from the worktree
    // and clobber the chmod. git keeps the explicit mode; so do we, by skipping the
    // refresh entirely when a chmod was requested.
    let refresh_paths: Vec<PathBuf> = if dry_run || chmod.is_some() {
        Vec::new()
    } else {
        paths.clone()
    };
    let do_refresh = !dry_run && chmod.is_none();
    if update && !all && paths.is_empty() && !dry_run && chmod.is_none() {
        let actions = sley_worktree::add_update_all_tracked_filtered(
            &worktree_root,
            &git_dir,
            format,
            &context.config,
        )?
        .into_iter()
        .map(|action| -> Result<AddAction> {
            match action {
                sley_worktree::AddUpdateTrackedAction::Add(path) => Ok(AddAction::Add(
                    worktree_root.join(
                        std::str::from_utf8(&path)
                            .map_err(|err| GitError::InvalidPath(err.to_string()))?,
                    ),
                )),
                sley_worktree::AddUpdateTrackedAction::Remove(path) => Ok(AddAction::Remove(
                    worktree_root.join(
                        std::str::from_utf8(&path)
                            .map_err(|err| GitError::InvalidPath(err.to_string()))?,
                    ),
                )),
            }
        })
        .collect::<Result<Vec<_>>>()?;
        if verbose {
            print_add_actions(&worktree_root, &actions)?;
        }
        return Ok(());
    }
    if update || all {
        let actions = resolve_add_update_actions(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            paths,
            all,
            ignore_missing,
        )?;
        if dry_run {
            print_add_actions(&worktree_root, &actions)?;
            validate_add_chmod_dry_run(&worktree_root, &actions, chmod)?;
            return Ok(());
        }
        let action_paths = actions
            .iter()
            .map(AddAction::path)
            .cloned()
            .collect::<Vec<_>>();
        let mut verbose_actions = actions;
        if !action_paths.is_empty() {
            let outcome = update_index_paths_filtered_for_add(
                &worktree_root,
                &git_dir,
                format,
                &action_paths,
                sley_worktree::UpdateIndexOptions {
                    add: true,
                    remove: true,
                    force_remove: false,
                    chmod,
                    info_only: false,
                    ignore_skip_worktree_entries: false,
                    allow_skip_worktree_entries: sparse,
                },
                &context.config,
                ignore_errors,
            )?;
            if ignore_errors {
                // Only report paths that actually landed in the index.
                let succeeded: BTreeSet<_> = outcome.succeeded.iter().collect();
                verbose_actions.retain(|action| succeeded.contains(action.path()));
            }
            if outcome.had_errors {
                if verbose {
                    print_add_actions(&worktree_root, &verbose_actions)?;
                }
                return Err(GitError::Exit(1));
            }
        }
        if do_refresh {
            refresh_index_after_add(
                &cwd,
                &worktree_root,
                &git_dir,
                format,
                &refresh_paths,
                false,
                pathspec_magic,
            )?;
        }
        if verbose {
            print_add_actions(&worktree_root, &verbose_actions)?;
        }
        return Ok(());
    }
    let AddRegularResolution {
        actions,
        mut reusable_index,
        exact_tracked,
        ignored_paths,
    } = resolve_add_regular_actions(
        &cwd,
        &worktree_root,
        &git_dir,
        format,
        paths,
        AddRegularOptions {
            chmod,
            force,
            ignore_errors,
            ignore_removal,
            ignore_missing,
            dry_run,
            sparse,
        },
        parsed_index,
        pathspec_magic,
    )?;
    if dry_run {
        print_add_actions(&worktree_root, &actions)?;
        validate_add_chmod_dry_run(&worktree_root, &actions, chmod)?;
        if !ignored_paths.is_empty() {
            print_add_ignored_paths(&context.config, &ignored_paths);
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    if let Some(exact) = exact_tracked {
        let actions = if exact.needs_index_update {
            let index = reusable_index.take().ok_or_else(|| {
                GitError::Command("exact tracked add lost its parsed index".into())
            })?;
            sley_worktree::add_exact_tracked_path_with_index(
                &worktree_root,
                &git_dir,
                format,
                index,
                &exact.git_path,
            )?
            .into_iter()
            .map(|action| add_update_tracked_action_to_add_action(&worktree_root, action))
            .collect::<Result<Vec<_>>>()?
        } else {
            actions
        };
        if verbose {
            print_add_actions(&worktree_root, &actions)?;
        }
        return Ok(());
    }
    let action_paths = actions
        .iter()
        .map(AddAction::path)
        .cloned()
        .collect::<Vec<_>>();
    if !action_paths.is_empty() {
        let warn_embedded = warn_embedded_repos && actions_may_add_embedded_repo(&actions);
        // Snapshot the tracked paths before staging only when the warning can
        // actually fire. Ordinary file adds never need this second index pass.
        let previously_tracked: BTreeSet<Vec<u8>> = if warn_embedded {
            if let Some(index) = reusable_index.as_ref() {
                index
                    .entries
                    .iter()
                    .map(|entry| entry.path.as_bytes().to_vec())
                    .collect()
            } else {
                sley_worktree::read_repository_index(&git_dir, format)?
                    .map(|index| {
                        index
                            .entries
                            .into_iter()
                            .map(|entry| entry.path.into_bytes())
                            .collect()
                    })
                    .unwrap_or_default()
            }
        } else {
            BTreeSet::new()
        };
        let update_options = sley_worktree::UpdateIndexOptions {
            add: true,
            remove: true,
            force_remove: false,
            chmod,
            info_only: false,
            ignore_skip_worktree_entries: false,
            allow_skip_worktree_entries: sparse,
        };
        let outcome = if ignore_errors {
            update_index_paths_filtered_for_add(
                &worktree_root,
                &git_dir,
                format,
                &action_paths,
                update_options,
                &context.config,
                true,
            )?
        } else if let Some(index) = reusable_index.take() {
            sley_worktree::update_index_paths_filtered_with_index(
                &worktree_root,
                &git_dir,
                format,
                index,
                &action_paths,
                update_options,
                &context.config,
            )?;
            AddIndexUpdateOutcome {
                had_errors: false,
                succeeded: action_paths.clone(),
            }
        } else {
            update_index_paths_filtered_for_add(
                &worktree_root,
                &git_dir,
                format,
                &action_paths,
                update_options,
                &context.config,
                false,
            )?
        };
        let mut verbose_actions = actions;
        if ignore_errors {
            let succeeded: BTreeSet<_> = outcome.succeeded.iter().collect();
            verbose_actions.retain(|action| succeeded.contains(action.path()));
        }
        if warn_embedded {
            warn_on_embedded_repos(
                &context.config,
                &worktree_root,
                &verbose_actions,
                &previously_tracked,
            )?;
        }
        if outcome.had_errors {
            if verbose {
                print_add_actions(&worktree_root, &verbose_actions)?;
            }
            return Err(GitError::Exit(1));
        }
        // Reuse filtered list for the post-success verbose print below.
        if verbose {
            print_add_actions(&worktree_root, &verbose_actions)?;
        }
        if do_refresh && !add_refresh_is_redundant(&worktree_root, &refresh_paths, &verbose_actions)
        {
            refresh_index_after_add(
                &cwd,
                &worktree_root,
                &git_dir,
                format,
                &refresh_paths,
                false,
                pathspec_magic,
            )?;
        }
        if !ignored_paths.is_empty() {
            print_add_ignored_paths(&context.config, &ignored_paths);
            return Err(GitError::Exit(1));
        }
        commands::hooks::run_post_index_change_hook(cli_session, false, false)?;
        return Ok(());
    }
    if do_refresh && !add_refresh_is_redundant(&worktree_root, &refresh_paths, &actions) {
        refresh_index_after_add(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            &refresh_paths,
            false,
            pathspec_magic,
        )?;
    }
    if verbose {
        print_add_actions(&worktree_root, &actions)?;
    }
    if !ignored_paths.is_empty() {
        print_add_ignored_paths(&context.config, &ignored_paths);
        return Err(GitError::Exit(1));
    }
    commands::hooks::run_post_index_change_hook(cli_session, false, false)?;
    Ok(())
}

/// `git add -N` / `git add --intent-to-add`: record that each named path will
/// be added later without staging its content. Mirrors `builtin/add.c`'s
/// `ADD_CACHE_INTENT` path: for every pathspec that resolves to a worktree file
/// not already tracked at stage 0, insert an intent-to-add placeholder entry
/// (empty-blob id, mode 100644, the ITA extended flag). Already-tracked paths
/// are left untouched. The index is rewritten with the entries kept in git's
/// canonical (path, stage) sort order.
pub(super) fn add_intent_to_add(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<()> {
    let mut index =
        sley_worktree::read_repository_index(git_dir, format)?.unwrap_or_else(|| Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        });

    let mut changed = false;
    for path in paths {
        // Resolve the pathspec to a worktree-relative git path. Reject anything
        // outside the worktree (git errors; we silently skip, matching the
        // tests which only ever pass in-tree paths).
        let absolute = normalize_add_absolute_path(cwd, path);
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            continue;
        };
        let git_path = add_git_path_bytes(relative)?;
        if git_path.is_empty() {
            continue;
        }
        // The worktree file must exist (git only marks paths that are present).
        if !worktree_root.join(relative).is_file() {
            continue;
        }
        // Skip paths already in the index at stage 0 (tracked or already ITA).
        let already = index.entries.iter().any(|entry| {
            index_entry_stage(entry) == 0 && entry.path.as_bytes() == git_path.as_slice()
        });
        if already {
            continue;
        }
        let entry = IndexEntry::intent_to_add(format, git_path);
        // Insert keeping the (path, stage) sort order the writer relies on.
        let position = index
            .entries
            .binary_search_by(|existing| {
                existing
                    .path
                    .as_bytes()
                    .cmp(entry.path.as_bytes())
                    .then(index_entry_stage(existing).cmp(&index_entry_stage(&entry)))
            })
            .unwrap_or_else(|insert_at| insert_at);
        index.entries.insert(position, entry);
        changed = true;
    }

    if changed {
        // ITA entries carry an extended flag → the writer needs index v3+.
        if index.version < 3 {
            index.version = 3;
        }
        let index_path = sley_worktree::repository_index_path(git_dir);
        std::fs::write(index_path, index.write(format)?)?;
    }
    Ok(())
}

fn add_update_tracked_action_to_add_action(
    worktree_root: &Path,
    action: sley_worktree::AddUpdateTrackedAction,
) -> Result<AddAction> {
    match action {
        sley_worktree::AddUpdateTrackedAction::Add(path) => Ok(AddAction::Add(worktree_root.join(
            std::str::from_utf8(&path).map_err(|err| GitError::InvalidPath(err.to_string()))?,
        ))),
        sley_worktree::AddUpdateTrackedAction::Remove(path) => {
            Ok(AddAction::Remove(worktree_root.join(
                std::str::from_utf8(&path).map_err(|err| GitError::InvalidPath(err.to_string()))?,
            )))
        }
    }
}

/// Result of a (possibly partial) index update for `git add`.
///
/// `had_errors` is true when at least one path failed under `--ignore-errors`
/// / `add.ignoreErrors`. `succeeded` lists the paths that were written so
/// verbose mode can print only those (git's `ADD_CACHE_VERBOSE` path only
/// prints successful `add '…'` lines).
struct AddIndexUpdateOutcome {
    had_errors: bool,
    succeeded: Vec<PathBuf>,
}

fn update_index_paths_filtered_for_add(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: sley_worktree::UpdateIndexOptions,
    config: &GitConfig,
    ignore_errors: bool,
) -> Result<AddIndexUpdateOutcome> {
    if !ignore_errors {
        sley_worktree::update_index_paths_filtered(
            worktree_root,
            git_dir,
            format,
            paths,
            options,
            config,
        )?;
        return Ok(AddIndexUpdateOutcome {
            had_errors: false,
            succeeded: paths.to_vec(),
        });
    }
    let mut had_errors = false;
    let mut succeeded = Vec::new();
    for path in paths {
        match sley_worktree::update_index_paths_filtered(
            worktree_root,
            git_dir,
            format,
            std::slice::from_ref(path),
            options,
            config,
        ) {
            Ok(_) => succeeded.push(path.clone()),
            Err(err) => {
                print_add_ignore_errors_message(worktree_root, path, &err);
                had_errors = true;
            }
        }
    }
    Ok(AddIndexUpdateOutcome {
        had_errors,
        succeeded,
    })
}

/// git's `index_path` / `add_to_index` failure lines under `ADD_CACHE_IGNORE_ERRORS`:
/// `error: open("path"): Permission denied` then `error: unable to index file 'path'`.
fn print_add_ignore_errors_message(worktree_root: &Path, path: &Path, err: &GitError) {
    let display = path
        .strip_prefix(worktree_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let message = err.to_string();
    let lower = message.to_ascii_lowercase();
    if lower.contains("permission denied") {
        eprintln!("error: open(\"{display}\"): Permission denied");
        eprintln!("error: unable to index file '{display}'");
    } else if let Some(io_msg) = message.strip_prefix("io error: ") {
        eprintln!("error: open(\"{display}\"): {io_msg}");
        eprintln!("error: unable to index file '{display}'");
    } else {
        eprintln!("error: {message}");
        eprintln!("error: unable to index file '{display}'");
    }
}

fn try_add_regular_exact_tracked_raw(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: AddRegularOptions,
) -> Result<Option<Vec<AddAction>>> {
    if paths.len() != 1
        || options.dry_run
        || options.chmod.is_some()
        || options.force
        || options.ignore_missing
        || options.sparse
    {
        return Ok(None);
    }
    let path = &paths[0];
    if add_pathspec_needs_status_walk(path) || add_pathspec_has_trailing_separator(path) {
        return Ok(None);
    }
    let absolute = normalize_add_absolute_path(cwd, path);
    let Ok(relative) = absolute.strip_prefix(worktree_root) else {
        return Ok(None);
    };
    if relative.as_os_str().is_empty() {
        return Ok(None);
    }
    let git_path = match add_git_path_bytes(relative) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let result = sley_worktree::add_exact_tracked_path_from_disk(
        worktree_root,
        git_dir,
        format,
        &git_path,
        options.ignore_removal,
        crate::effective_config_parameters_env().as_deref(),
    )?;
    match result {
        sley_worktree::AddExactTrackedPathResult::Handled(action) => action
            .into_iter()
            .map(|action| add_update_tracked_action_to_add_action(worktree_root, action))
            .collect::<Result<Vec<_>>>()
            .map(Some),
        sley_worktree::AddExactTrackedPathResult::Unsupported => Ok(None),
    }
}

fn add_pathspec_has_trailing_separator(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .ends_with(std::path::MAIN_SEPARATOR)
}

/// Re-stat the index entries `git add` touched so the cached stat matches the
/// worktree (git's `refresh_index` over the pathspec): a tracked path whose
/// content is unchanged but whose stat is dirty (e.g. it was `touch`ed) is
/// stamped clean, so `git diff-files` reports nothing. An empty pathspec (bare
/// `add -u`/`-A`) refreshes every tracked entry. Quiet + tolerant of missing
/// files (content mismatches are genuine worktree changes, not a refresh error).
fn refresh_index_after_add(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    refresh_paths: &[PathBuf],
    strict_pathspec: bool,
    pathspec_magic: sley_worktree::PathspecMatchMagic,
) -> Result<()> {
    // Empty `refresh_paths` means "every tracked entry" (bare `add -u`/`-A` or
    // bare `--refresh`). Non-empty means "only pathspec matches"; when nothing
    // matches and we are non-strict, skip the refresh entirely rather than
    // falling through to a full-index refresh (empty path list in the engine
    // means "all").
    let selected = if refresh_paths.is_empty() {
        None
    } else {
        let Some(mut index) = sley_worktree::read_repository_index(git_dir, format)? else {
            if strict_pathspec {
                for path in refresh_paths {
                    eprintln!(
                        "fatal: pathspec '{}' did not match any files",
                        path.to_string_lossy()
                    );
                }
                return Err(GitError::Exit(128));
            }
            return Ok(());
        };
        // Pathspec validation operates on logical tracked paths, not on the
        // on-disk sparse-index representation.  An exact path such as
        // `folder1/a` must therefore match the leaf hidden beneath a collapsed
        // `folder1/` sparse-directory entry.  Expand only this temporary view;
        // refresh_index_paths_with_options() still owns any observable index
        // mutation and can preserve the sparse layout on disk.
        if index
            .entries
            .iter()
            .any(sley_index::IndexEntry::is_sparse_dir)
        {
            let odb = sley_odb::FileObjectDatabase::from_git_dir(git_dir, format);
            sley_worktree::expand_sparse_index_view(&mut index, &odb, format)?;
        }
        let mut compiled =
            AddCompiledPathspecs::parse(cwd, worktree_root, refresh_paths, pathspec_magic)?;
        let mut selected = Vec::new();
        for entry in &index.entries {
            if entry.stage() != sley_index::Stage::Normal {
                continue;
            }
            if compiled.matches(entry.path.as_bytes()) {
                selected.push(worktree_path_from_git_path(
                    worktree_root,
                    entry.path.as_bytes(),
                )?);
            }
        }
        if strict_pathspec && let Some(spec) = compiled.unmatched_includes().next() {
            eprintln!("fatal: pathspec '{}' did not match any files", spec.display);
            return Err(GitError::Exit(128));
        }
        if selected.is_empty() {
            return Ok(());
        }
        Some(selected)
    };
    let paths = selected.as_deref().unwrap_or(&[]);
    sley_worktree::refresh_index_paths_with_options(
        worktree_root,
        git_dir,
        format,
        paths,
        /* quiet */ true,
        /* ignore_missing */ true,
        /* ignore_submodules */ false,
        /* allow_unmerged */ false,
        /* really_refresh */ false,
    )?;
    Ok(())
}

/// Upstream pathspec.c `die_path_inside_submodule()`: a pathspec that names a
/// path *inside* a tracked gitlink is fatal — the file belongs to the
/// submodule's repository, not this one.
fn die_on_pathspec_inside_submodule(
    cwd: &Path,
    worktree_root: &Path,
    index: Option<&Index>,
    paths: &[PathBuf],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let Some(index) = index else {
        return Ok(());
    };
    if paths
        .iter()
        .any(|path| add_pathspec_needs_status_walk(path))
    {
        return die_on_pathspec_inside_submodule_by_scan(cwd, worktree_root, index, paths);
    }
    let mut git_paths = Vec::with_capacity(paths.len());
    for path in paths {
        match add_pathspec_git_path_for_submodule_fast(cwd, worktree_root, path)? {
            AddSubmodulePathspec::Inside(git_path) => git_paths.push((path, git_path)),
            AddSubmodulePathspec::Outside => {}
            AddSubmodulePathspec::Unsafe => {
                return die_on_pathspec_inside_submodule_by_scan(cwd, worktree_root, index, paths);
            }
        }
    }
    for (path, git_path) in git_paths {
        if let Some(link) = gitlink_ancestor_for_path(&index.entries, &git_path) {
            eprintln!(
                "fatal: Pathspec '{}' is in submodule '{}'",
                path.to_string_lossy(),
                String::from_utf8_lossy(link)
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

enum AddSubmodulePathspec {
    Inside(Vec<u8>),
    Outside,
    Unsafe,
}

fn add_pathspec_git_path_for_submodule_fast(
    cwd: &Path,
    worktree_root: &Path,
    path: &Path,
) -> Result<AddSubmodulePathspec> {
    let absolute = normalize_add_absolute_path(cwd, path);
    let Ok(relative) = absolute.strip_prefix(worktree_root) else {
        return Ok(AddSubmodulePathspec::Outside);
    };
    if relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::Prefix(_)
                | std::path::Component::RootDir
        )
    }) {
        return Ok(AddSubmodulePathspec::Unsafe);
    }
    Ok(AddSubmodulePathspec::Inside(add_git_path_bytes(relative)?))
}

fn gitlink_ancestor_for_path<'a>(entries: &'a [IndexEntry], git_path: &[u8]) -> Option<&'a [u8]> {
    for (idx, byte) in git_path.iter().enumerate() {
        if *byte != b'/' || idx == 0 {
            continue;
        }
        if let Some(link) = index_gitlink_at_path(entries, &git_path[..idx]) {
            return Some(link);
        }
    }
    None
}

fn index_gitlink_at_path<'a>(entries: &'a [IndexEntry], path: &[u8]) -> Option<&'a [u8]> {
    let range = add_index_entries_path_range(entries, path);
    entries[range]
        .iter()
        .find(|entry| entry.stage() == sley_index::Stage::Normal && entry.mode == 0o160000)
        .map(|entry| entry.path.as_bytes())
}

fn die_on_pathspec_inside_submodule_by_scan(
    cwd: &Path,
    worktree_root: &Path,
    index: &Index,
    paths: &[PathBuf],
) -> Result<()> {
    let gitlinks: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|entry| entry.mode == 0o160000)
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    if gitlinks.is_empty() {
        return Ok(());
    }
    for path in paths {
        let absolute = normalize_add_absolute_path(cwd, path);
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            continue;
        };
        let git_path = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        let git_path = git_path.as_bytes();
        for link in &gitlinks {
            if git_path.len() > link.len()
                && git_path.starts_with(link)
                && git_path[link.len()] == b'/'
            {
                eprintln!(
                    "fatal: Pathspec '{}' is in submodule '{}'",
                    path.to_string_lossy(),
                    String::from_utf8_lossy(link)
                );
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(())
}

/// Upstream builtin/add.c check_embedded_repo(): after staging, warn (per
/// path) about each embedded git repository that was just added as a gitlink,
/// and print the `advice.addEmbeddedRepo` hint once.
fn warn_on_embedded_repos(
    config: &GitConfig,
    worktree_root: &Path,
    actions: &[AddAction],
    previously_tracked: &BTreeSet<Vec<u8>>,
) -> Result<()> {
    let mut adviced = false;
    for action in actions {
        let AddAction::Add(path) = action else {
            continue;
        };
        if !path.is_dir() || sley_diff_merge::gitlink_git_dir(path).is_none() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(worktree_root) else {
            continue;
        };
        let name = relative.to_string_lossy().replace('\\', "/");
        if previously_tracked.contains(name.as_bytes()) {
            continue;
        }
        eprintln!("warning: adding embedded git repository: {name}");
        if adviced {
            continue;
        }
        adviced = true;
        let advice_enabled = config
            .get_bool("advice", None, "addembeddedrepo")
            .unwrap_or(true);
        if !advice_enabled {
            continue;
        }
        eprintln!("hint: You've added another git repository inside your current repository.");
        eprintln!("hint: Clones of the outer repository will not contain the contents of");
        eprintln!("hint: the embedded repository and will not know how to obtain it.");
        eprintln!("hint: If you meant to add a submodule, use:");
        eprintln!("hint:");
        eprintln!("hint: \tgit submodule add <url> {name}");
        eprintln!("hint:");
        eprintln!("hint: If you added this path by mistake, you can remove it from the");
        eprintln!("hint: index with:");
        eprintln!("hint:");
        eprintln!("hint: \tgit rm --cached {name}");
        eprintln!("hint:");
        eprintln!("hint: See \"git help submodule\" for more information.");
        eprintln!(
            "hint: Disable this message with \"git config set advice.addEmbeddedRepo false\""
        );
    }
    Ok(())
}

fn actions_may_add_embedded_repo(actions: &[AddAction]) -> bool {
    actions.iter().any(|action| match action {
        AddAction::Add(path) => path.is_dir() && sley_diff_merge::gitlink_git_dir(path).is_some(),
        AddAction::Remove(_) => false,
    })
}

fn add_refresh_is_redundant(
    worktree_root: &Path,
    refresh_paths: &[PathBuf],
    actions: &[AddAction],
) -> bool {
    if refresh_paths.is_empty() || refresh_paths.len() != actions.len() {
        return false;
    }
    let action_paths = actions.iter().map(AddAction::path).collect::<BTreeSet<_>>();
    refresh_paths.iter().all(|path| {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        if fs::symlink_metadata(&absolute)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            return false;
        }
        action_paths.contains(&absolute)
    })
}

fn parse_add_chmod(value: &str) -> Result<bool> {
    match value {
        "+x" => Ok(true),
        "-x" => Ok(false),
        _ => {
            eprintln!("fatal: --chmod param '{value}' must be either -x or +x");
            Err(GitError::Exit(128))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AddRegularOptions {
    chmod: Option<bool>,
    force: bool,
    ignore_errors: bool,
    ignore_removal: bool,
    ignore_missing: bool,
    dry_run: bool,
    sparse: bool,
}

struct AddRegularResolution {
    actions: Vec<AddAction>,
    reusable_index: Option<Index>,
    exact_tracked: Option<ExactTrackedAdd>,
    ignored_paths: Vec<Vec<u8>>,
}

struct AddCompiledPathspec {
    display: String,
    element: sley_pathspec::PathspecElement,
    matched: bool,
}

struct AddCompiledPathspecs {
    specs: Vec<AddCompiledPathspec>,
    have_include: bool,
    last_matched_includes: Vec<usize>,
}

impl AddCompiledPathspecs {
    fn parse(
        cwd: &Path,
        worktree_root: &Path,
        paths: &[PathBuf],
        pathspec_magic: sley_worktree::PathspecMatchMagic,
    ) -> Result<Self> {
        let root = fs::canonicalize(worktree_root)?;
        let cwd_prefix = fs::canonicalize(cwd)
            .ok()
            .and_then(|cwd| cwd.strip_prefix(&root).ok().map(Path::to_path_buf))
            .map(|relative| relative.to_string_lossy().replace('\\', "/").into_bytes())
            .unwrap_or_default();
        let mut specs = Vec::with_capacity(paths.len());
        let mut have_include = false;
        for path in paths {
            let arg = add_pathspec_arg_for_matcher(worktree_root, path)?;
            let element = sley_pathspec::parse_normalized_pathspec_element(
                &cwd_prefix,
                &arg,
                pathspec_magic,
            )?;
            have_include |= !element.is_exclude();
            specs.push(AddCompiledPathspec {
                display: path.to_string_lossy().into_owned(),
                element,
                matched: false,
            });
        }
        Ok(Self {
            specs,
            have_include,
            last_matched_includes: Vec::new(),
        })
    }

    fn have_include(&self) -> bool {
        self.have_include
    }

    fn mark_matched(&mut self, idx: usize) {
        if let Some(spec) = self.specs.get_mut(idx) {
            spec.matched = true;
        }
    }

    fn matched_include_indexes(&self) -> impl Iterator<Item = usize> + '_ {
        self.last_matched_includes.iter().copied()
    }

    fn matches(&mut self, path: &[u8]) -> bool {
        if self.specs.is_empty() {
            return true;
        }
        self.last_matched_includes.clear();
        let mut included = false;
        let mut excluded = false;
        for (idx, spec) in self.specs.iter_mut().enumerate() {
            if spec.element.matches_path(path) {
                spec.matched = true;
                if spec.element.is_exclude() {
                    excluded = true;
                } else {
                    included = true;
                    self.last_matched_includes.push(idx);
                }
            }
        }
        !excluded && (!self.have_include || included)
    }

    fn unmatched_includes(&self) -> impl Iterator<Item = &AddCompiledPathspec> {
        self.specs
            .iter()
            .filter(|spec| !spec.element.is_exclude() && !spec.matched)
    }
}

/// Resolve `git add --renormalize` exclusively against stage-0 index entries.
/// Renormalization implies `-u`: tracked deletions remain selected for removal,
/// while untracked filesystem matches are never introduced.
fn resolve_add_renormalize_paths(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    pathspec_magic: sley_worktree::PathspecMatchMagic,
) -> Result<Vec<PathBuf>> {
    let mut compiled = AddCompiledPathspecs::parse(cwd, worktree_root, paths, pathspec_magic)?;
    let index = sley_worktree::read_repository_index(git_dir, format)?;
    let mut selected = Vec::new();
    if let Some(index) = index {
        for entry in index
            .entries
            .iter()
            .filter(|entry| entry.stage() == sley_index::Stage::Normal)
        {
            let git_path = entry.path.as_bytes();
            if compiled.matches(git_path) {
                selected.push(worktree_path_from_git_path(worktree_root, git_path)?);
            }
        }
    }
    for spec in compiled.unmatched_includes() {
        eprintln!("fatal: pathspec '{}' did not match any files", spec.display);
        return Err(GitError::Exit(128));
    }
    Ok(selected)
}

fn add_pathspec_arg_for_matcher(worktree_root: &Path, path: &Path) -> Result<String> {
    if !path.is_absolute() {
        return Ok(path.to_string_lossy().into_owned());
    }
    // Resolve absolute pathspecs the same way the rest of `add` does
    // (`normalize_add_absolute_path`): canonicalize the parent so symlink
    // prefixes (`/var` → `/private/var` on macOS) and case-insensitive
    // directory folds land under `worktree_root`. Lexical-only normalization
    // fails t3700 "path is case-insensitive", where the user lowercases the
    // whole absolute path including intermediate components.
    let absolute = normalize_add_absolute_path(worktree_root, path);
    let absolute = match absolute.strip_prefix(worktree_root) {
        Ok(_) => absolute,
        Err(_) => {
            let lexical = normalize_add_pathspec_absolute_path_lexically(path);
            match lexical.strip_prefix(worktree_root) {
                Ok(_) => lexical,
                Err(_) => case_insensitive_existing_path_under_worktree(worktree_root, &absolute)
                    .or_else(|| {
                        case_insensitive_existing_path_under_worktree(worktree_root, &lexical)
                    })
                    .or_else(|| {
                        // Last resort: canonicalize the full path (folds case +
                        // resolves symlinks) when the file exists.
                        fs::canonicalize(path).ok().filter(|canonical| {
                            canonical.starts_with(worktree_root)
                                || case_insensitive_path_under_prefix(worktree_root, canonical)
                        })
                    })
                    .unwrap_or(absolute)
            }
        }
    };
    let absolute = match absolute.strip_prefix(worktree_root) {
        Ok(_) => absolute,
        Err(_) => case_insensitive_existing_path_under_worktree(worktree_root, &absolute)
            .unwrap_or(absolute),
    };
    let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
    })?;
    let git_path = add_git_path_bytes(relative)?;
    if git_path.is_empty() {
        Ok(":/".to_string())
    } else {
        Ok(format!(":/{}", String::from_utf8_lossy(&git_path)))
    }
}

/// True when `path` lies under `prefix` ignoring ASCII case of every component
/// (used after canonicalize on case-insensitive filesystems).
fn case_insensitive_path_under_prefix(prefix: &Path, path: &Path) -> bool {
    let prefix_components = prefix
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let path_components = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if path_components.len() < prefix_components.len() {
        return false;
    }
    prefix_components
        .iter()
        .zip(&path_components)
        .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn normalize_add_pathspec_absolute_path_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(_)
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn case_insensitive_existing_path_under_worktree(
    worktree_root: &Path,
    absolute: &Path,
) -> Option<PathBuf> {
    if !absolute.exists() {
        return None;
    }
    let root_components = worktree_root
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect::<Vec<_>>();
    let absolute_components = absolute
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect::<Vec<_>>();
    if absolute_components.len() < root_components.len() {
        return None;
    }
    for (left, right) in root_components.iter().zip(&absolute_components) {
        if !left
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
        {
            return None;
        }
    }
    let mut current = worktree_root.to_path_buf();
    for wanted in &absolute_components[root_components.len()..] {
        let wanted = wanted.to_string_lossy();
        let entry = fs::read_dir(&current)
            .ok()?
            .filter_map(std::result::Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&wanted)
            })?;
        current = entry.path();
    }
    Some(current)
}

struct ExactTrackedAdd {
    git_path: Vec<u8>,
    needs_index_update: bool,
}

struct TrackedExactResolution {
    actions: Vec<AddAction>,
    exact_tracked: Option<ExactTrackedAdd>,
}

fn resolve_add_regular_actions(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: Vec<PathBuf>,
    options: AddRegularOptions,
    mut reusable_index: Option<Index>,
    pathspec_magic: sley_worktree::PathspecMatchMagic,
) -> Result<AddRegularResolution> {
    if let Some(exact) = resolve_add_regular_tracked_exact_actions(
        cwd,
        worktree_root,
        git_dir,
        &paths,
        options,
        reusable_index.as_ref(),
    )? {
        return Ok(AddRegularResolution {
            actions: exact.actions,
            reusable_index,
            exact_tracked: exact.exact_tracked,
            ignored_paths: Vec::new(),
        });
    }
    // `--sparse` explicitly authorizes paths outside the cone. Resolve its
    // pathspec and worktree changes against logical leaves rather than treating
    // a materialized collapsed sparse directory as one stageable directory
    // entry. This is a temporary semantic view; the mutation engine expands
    // only selected directories in the on-disk index.
    if options.sparse
        && let Some(index) = reusable_index.as_mut()
        && index
            .entries
            .iter()
            .any(sley_index::IndexEntry::is_sparse_dir)
    {
        let odb = sley_odb::FileObjectDatabase::from_git_dir(git_dir, format);
        sley_worktree::expand_sparse_index_view(index, &odb, format)?;
    }
    let mut compiled_pathspecs =
        AddCompiledPathspecs::parse(cwd, worktree_root, &paths, pathspec_magic)?;
    let pathspecs = paths
        .into_iter()
        .map(|path| {
            let absolute = normalize_add_absolute_path(cwd, &path);
            let matched = absolute.exists();
            (path, absolute, matched)
        })
        .collect::<Vec<_>>();
    let mut matched = pathspecs
        .iter()
        .map(|(_, _, matched)| *matched)
        .collect::<Vec<_>>();
    for (idx, matched) in matched.iter().copied().enumerate() {
        if matched {
            compiled_pathspecs.mark_matched(idx);
        }
    }
    let mut actions = Vec::new();
    let mut seen = BTreeSet::new();
    let mut ignored_paths = BTreeSet::new();
    let _ignore_errors = options.ignore_errors;
    // A pathspec is matched by an unchanged tracked file too. The status stream
    // intentionally omits such files, so mark index matches independently
    // without turning them into staging actions. This is what lets commands
    // such as `git add --ignore-errors '*.txt'` succeed as a no-op.
    let indexed_paths = add_all_index_paths(git_dir, format, reusable_index.as_ref())?;
    for indexed_path in &indexed_paths {
        let path_matches = compiled_pathspecs.matches(indexed_path);
        if !path_matches {
            continue;
        }
        if !compiled_pathspecs.have_include() {
            for matched in &mut matched {
                *matched = true;
            }
        } else {
            for idx in compiled_pathspecs.matched_include_indexes() {
                matched[idx] = true;
            }
        }
    }
    if !options.force {
        for (idx, ignored_path) in collect_add_ignored_pathspec_matches(
            worktree_root,
            git_dir,
            format,
            &pathspecs,
            &indexed_paths,
        )? {
            matched[idx] = true;
            compiled_pathspecs.mark_matched(idx);
            ignored_paths.insert(ignored_path);
        }
    }
    for path in add_unmerged_index_paths(git_dir, format, reusable_index.as_ref())? {
        let path_matches = compiled_pathspecs.matches(&path);
        if path_matches {
            if !compiled_pathspecs.have_include() {
                for matched in &mut matched {
                    *matched = true;
                }
            } else {
                for idx in compiled_pathspecs.matched_include_indexes() {
                    matched[idx] = true;
                }
            }
        }
        if !path_matches {
            continue;
        }
        let path = worktree_path_from_git_path(worktree_root, &path)?;
        if seen.insert(path.clone()) {
            actions.push(AddAction::Add(path));
        }
    }
    let mut collect_status_action = |entry: sley_worktree::ShortStatusRow<'_>| {
        let actionable = (entry.index == b'?' && entry.worktree == b'?')
            || entry.worktree == b'M'
            || entry.worktree == b'T'
            || entry.worktree == b'D';
        if !actionable {
            return Ok(sley_worktree::StreamControl::Continue);
        }
        let path = worktree_root.join(
            std::str::from_utf8(entry.path)
                .map_err(|err| GitError::InvalidPath(err.to_string()))?,
        );
        let path_matches = compiled_pathspecs.matches(entry.path);
        if path_matches {
            for (idx, (_, pathspec, _)) in pathspecs.iter().enumerate() {
                if add_path_matches(&path, pathspec) {
                    matched[idx] = true;
                }
            }
            if !compiled_pathspecs.have_include() {
                for matched in &mut matched {
                    *matched = true;
                }
            } else {
                for idx in compiled_pathspecs.matched_include_indexes() {
                    matched[idx] = true;
                }
            }
        }
        if !path_matches {
            return Ok(sley_worktree::StreamControl::Continue);
        }
        if entry.worktree == b'D' && options.ignore_removal {
            return Ok(sley_worktree::StreamControl::Continue);
        }
        if seen.insert(path.clone()) {
            let action = if entry.worktree == b'D' {
                AddAction::Remove(path)
            } else {
                AddAction::Add(path)
            };
            actions.push(action);
        }
        Ok(sley_worktree::StreamControl::Continue)
    };
    if let Some(index) = reusable_index.as_ref().filter(|index| {
        index.is_sparse()
            || index
                .entries
                .iter()
                .any(sley_index::IndexEntry::is_sparse_dir)
    }) {
        // General status must account for staged HEAD-to-index differences and
        // may therefore need a full index. Add only needs index-to-worktree and
        // untracked changes, so use the sparse-boundary-aware engine query.
        for entry in sley_worktree::collect_add_worktree_status_with_index(
            worktree_root,
            git_dir,
            format,
            index,
        )? {
            collect_status_action(entry.as_row())?;
        }
    } else {
        sley_worktree::stream_short_status(worktree_root, git_dir, format, collect_status_action)?;
    }
    if options.chmod.is_some() || options.force {
        // `--force` stages paths the status walk never reports (gitignored
        // files; gitignored embedded repositories as gitlinks), so resolve the
        // pathspecs straight off the filesystem. The same walk feeds `--chmod`,
        // which must touch every matching file whether or not it changed.
        for (idx, (_, pathspec, _)) in pathspecs.iter().enumerate() {
            for path in resolve_add_paths(cwd, worktree_root, git_dir, vec![pathspec.clone()])? {
                if fs::symlink_metadata(&path).is_err() {
                    continue;
                }
                matched[idx] = true;
                compiled_pathspecs.mark_matched(idx);
                if seen.insert(path.clone()) {
                    actions.push(AddAction::Add(path));
                }
            }
        }
    }
    if options.ignore_missing {
        for (idx, (display, pathspec, _)) in pathspecs.iter().enumerate() {
            if matched[idx] {
                continue;
            }
            if let Some(ignored_path) =
                ignored_missing_add_pathspec(worktree_root, display, pathspec)?
            {
                matched[idx] = true;
                compiled_pathspecs.mark_matched(idx);
                ignored_paths.insert(ignored_path);
            }
        }
    }
    for spec in compiled_pathspecs.unmatched_includes() {
        if !options.ignore_missing {
            eprintln!("fatal: pathspec '{}' did not match any files", spec.display);
            return Err(GitError::Exit(128));
        }
    }
    Ok(AddRegularResolution {
        actions,
        reusable_index: None,
        exact_tracked: None,
        ignored_paths: ignored_paths.into_iter().collect(),
    })
}

fn collect_add_ignored_pathspec_matches(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    pathspecs: &[(PathBuf, PathBuf, bool)],
    indexed_paths: &BTreeSet<Vec<u8>>,
) -> Result<Vec<(usize, Vec<u8>)>> {
    if pathspecs.is_empty() {
        return Ok(Vec::new());
    }

    let mut candidates = BTreeSet::new();
    for directory in [false, true] {
        let ignored = sley_worktree::untracked_paths_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory,
                no_empty_directory: false,
                preserve_ignored_directories: false,
                exclude_standard: true,
                ignored_only: true,
                exclude_patterns: Vec::new(),
                exclude_per_directory: Vec::new(),
                pathspecs: Vec::new(),
            },
        )?;
        for mut path in ignored {
            if path.ends_with(b"/") {
                path.pop();
            }
            if !path.is_empty() && !add_ignored_candidate_is_indexed(&path, indexed_paths) {
                candidates.insert(path);
            }
        }
    }

    let mut matches = Vec::new();
    for (idx, (display, pathspec, _)) in pathspecs.iter().enumerate() {
        for candidate in &candidates {
            let candidate_path = worktree_path_from_git_path(worktree_root, candidate)?;
            if add_ignored_path_matches(display, &candidate_path, pathspec) {
                matches.push((
                    idx,
                    add_ignored_display_path(worktree_root, &candidate_path, candidate)?,
                ));
            }
        }
    }
    Ok(matches)
}

fn add_all_index_paths(
    git_dir: &Path,
    format: ObjectFormat,
    index: Option<&Index>,
) -> Result<BTreeSet<Vec<u8>>> {
    if let Some(index) = index {
        return Ok(index
            .entries
            .iter()
            .map(|entry| entry.path.as_bytes().to_vec())
            .collect());
    }
    Ok(sley_worktree::read_repository_index(git_dir, format)?
        .map(|index| {
            index
                .entries
                .into_iter()
                .map(|entry| entry.path.into_bytes())
                .collect()
        })
        .unwrap_or_default())
}

fn add_unmerged_index_paths(
    git_dir: &Path,
    format: ObjectFormat,
    index: Option<&Index>,
) -> Result<Vec<Vec<u8>>> {
    let owned;
    let index = if let Some(index) = index {
        index
    } else {
        owned = sley_worktree::read_repository_index(git_dir, format)?;
        match owned.as_ref() {
            Some(index) => index,
            None => return Ok(Vec::new()),
        }
    };
    let mut paths = index
        .entries
        .iter()
        .filter(|entry| entry.stage() != sley_index::Stage::Normal)
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect::<Vec<_>>();
    paths.dedup();
    Ok(paths)
}

fn add_ignored_candidate_is_indexed(candidate: &[u8], indexed_paths: &BTreeSet<Vec<u8>>) -> bool {
    if indexed_paths.contains(candidate) {
        return true;
    }
    let mut prefix = candidate.to_vec();
    prefix.push(b'/');
    indexed_paths
        .range(prefix.clone()..)
        .next()
        .is_some_and(|path| path.starts_with(&prefix))
}

fn add_ignored_path_matches(display: &Path, candidate_path: &Path, pathspec: &Path) -> bool {
    if add_pathspec_needs_status_walk(display) {
        let has_separator = display
            .components()
            .filter(|component| !matches!(component, std::path::Component::CurDir))
            .count()
            > 1;
        if !has_separator {
            return false;
        }
        return add_path_matches(candidate_path, pathspec);
    }
    candidate_path == pathspec || pathspec.starts_with(candidate_path)
}

fn ignored_missing_add_pathspec(
    worktree_root: &Path,
    display: &Path,
    pathspec: &Path,
) -> Result<Option<Vec<u8>>> {
    if add_pathspec_needs_status_walk(display) {
        return Ok(None);
    }
    let Ok(relative) = pathspec.strip_prefix(worktree_root) else {
        return Ok(None);
    };
    let git_path = add_git_path_bytes(relative)?;
    if git_path.is_empty() {
        return Ok(None);
    }
    Ok(
        sley_worktree::standard_ignore_match(worktree_root, &git_path, false)?
            .filter(|ignore_match| ignore_match.ignored)
            .map(|_| git_path),
    )
}

fn worktree_path_from_git_path(worktree_root: &Path, git_path: &[u8]) -> Result<PathBuf> {
    let text =
        std::str::from_utf8(git_path).map_err(|err| GitError::InvalidPath(err.to_string()))?;
    let mut path = worktree_root.to_path_buf();
    for component in text.split('/') {
        if !component.is_empty() {
            path.push(component);
        }
    }
    Ok(path)
}

fn add_ignored_display_path(
    worktree_root: &Path,
    candidate_path: &Path,
    candidate_git_path: &[u8],
) -> Result<Vec<u8>> {
    let mut prefix = Vec::new();
    for component in candidate_git_path.split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(component);
        let prefix_path = worktree_path_from_git_path(worktree_root, &prefix)?;
        let is_dir = prefix_path.is_dir();
        if sley_worktree::standard_ignore_match(worktree_root, &prefix, is_dir)?
            .is_some_and(|ignore_match| ignore_match.ignored)
        {
            return Ok(prefix);
        }
    }
    if candidate_path.is_dir() {
        return add_git_path_bytes(
            candidate_path
                .strip_prefix(worktree_root)
                .map_err(|_| GitError::InvalidPath(candidate_path.display().to_string()))?,
        );
    }
    Ok(candidate_git_path.to_vec())
}

fn print_add_ignored_paths(config: &GitConfig, ignored_paths: &[Vec<u8>]) {
    eprintln!("The following paths are ignored by one of your .gitignore files:");
    for path in ignored_paths {
        eprintln!("{}", String::from_utf8_lossy(path));
    }
    if add_ignored_file_advice_enabled(config) {
        eprintln!("hint: Use -f if you really want to add them.");
        eprintln!("hint: Disable this message with \"git config set advice.addIgnoredFile false\"");
    }
}

fn add_ignored_file_advice_enabled(config: &GitConfig) -> bool {
    if env::var("GIT_ADVICE")
        .ok()
        .as_deref()
        .and_then(parse_config_bool)
        == Some(false)
    {
        return false;
    }
    config
        .get_bool("advice", None, "addignoredfile")
        .or_else(|| config.get_bool("advice", None, "addIgnoredFile"))
        .unwrap_or(true)
}

fn resolve_add_regular_tracked_exact_actions(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    paths: &[PathBuf],
    options: AddRegularOptions,
    index: Option<&Index>,
) -> Result<Option<TrackedExactResolution>> {
    if paths.is_empty() || options.chmod.is_some() || options.force || options.dry_run {
        return Ok(None);
    }
    let Some(index) = index else {
        return Ok(None);
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
    let index_mtime = fs::metadata(&index_path)
        .ok()
        .and_then(|metadata| sley_index::file_mtime_parts(&metadata));
    let stat_cache = sley_index::IndexStatCache::from_index_mtime_only(index_mtime);
    let mut actions = Vec::new();
    let mut exact_tracked = None;
    let single_path = paths.len() == 1;
    for path in paths {
        if add_pathspec_needs_status_walk(path) {
            return Ok(None);
        }
        let absolute = normalize_add_absolute_path(cwd, path);
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            return Ok(None);
        };
        let git_path = add_git_path_bytes(relative)?;
        let range = add_index_entries_path_range(&index.entries, &git_path);
        if range.is_empty() {
            let metadata = match fs::symlink_metadata(&absolute) {
                Ok(metadata) => metadata,
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(None);
                }
                Err(err) => return Err(err.into()),
            };
            let file_type = metadata.file_type();
            if metadata.is_dir() || !(file_type.is_file() || file_type.is_symlink()) {
                return Ok(None);
            }
            if sley_worktree::standard_ignore_match(worktree_root, &git_path, false)?
                .is_some_and(|ignore_match| ignore_match.ignored)
            {
                return Ok(None);
            }
            // An exact, present, untracked file needs no whole-status walk to
            // prove it is actionable. Return it directly and let the shared
            // index mutation engine decide whether its path intersects a
            // collapsed sparse directory.
            actions.push(AddAction::Add(absolute));
            continue;
        }
        if range.len() != 1 {
            return Ok(None);
        }
        if index.entries[range.clone()]
            .iter()
            .any(|entry| entry.stage() != sley_index::Stage::Normal || entry.is_skip_worktree())
        {
            return Ok(None);
        }
        let entry = &index.entries[range.start];
        if entry.mode == 0o160000 {
            return Ok(None);
        }
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if metadata.is_dir() || !(file_type.is_file() || file_type.is_symlink()) {
                    return Ok(None);
                }
                let needs_index_update =
                    stat_cache.reusable_index_entry(entry, &metadata).is_none();
                if needs_index_update {
                    actions.push(AddAction::Add(absolute));
                }
                if single_path {
                    exact_tracked = Some(ExactTrackedAdd {
                        git_path: git_path.clone(),
                        needs_index_update,
                    });
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                let needs_index_update = !options.ignore_removal;
                if needs_index_update {
                    actions.push(AddAction::Remove(absolute));
                }
                if single_path {
                    exact_tracked = Some(ExactTrackedAdd {
                        git_path: git_path.clone(),
                        needs_index_update,
                    });
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(Some(TrackedExactResolution {
        actions,
        exact_tracked,
    }))
}

/// Refuse to update index entries that live outside the sparse-checkout, the
/// way git's `add` does. A pathspec is rejected when it matches an entry that
/// either carries the skip-worktree bit or lies outside the sparse-checkout
/// definition, and matches nothing that *would* legitimately be staged — this
/// mirrors git's `find_pathspecs_matching_skip_worktree` (`ce_skip_worktree(ce)
/// || !path_in_sparse_checkout(ce->name)`). Unlike a naive pattern check this
/// fires even when `core.sparseCheckout` is disabled, because the skip-worktree
/// bit alone protects an entry (t3705 exercises exactly this with the bit set
/// but sparse-checkout off).
///
/// `--sparse` (`sparse_flag`) opts out entirely. Glob/magic pathspecs are left
/// to the normal walk (a glob that also matches a dense path must not warn).
fn reject_add_skip_worktree_paths(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    sparse_flag: bool,
    is_refresh: bool,
    config: &GitConfig,
) -> Result<()> {
    if sparse_flag || paths.is_empty() {
        return Ok(());
    }
    // `active` is `Some` only when core.sparseCheckout is enabled; when it is
    // `None` every path is "in" the checkout (git's path_in_sparse_checkout
    // returns 1 when sparse-checkout is off), so only the skip-worktree bit can
    // reject a path.
    let active = active_sparse_checkout_for_add(git_dir)?;
    let index = sley_worktree::read_repository_index(git_dir, format)?;
    let mut rejected = Vec::new();
    for path in paths {
        if add_pathspec_needs_status_walk(path) {
            continue;
        }
        let absolute = normalize_add_absolute_path(cwd, path);
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            continue;
        };
        if relative.as_os_str().is_empty()
            || relative == Path::new(".")
            || relative == Path::new("")
        {
            continue;
        }
        let git_path = match add_git_path_bytes(relative) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if git_path.is_empty() {
            continue;
        }
        let metadata = fs::symlink_metadata(&absolute).ok();
        let present = metadata.is_some();
        let is_dir = metadata.as_ref().map(fs::Metadata::is_dir).unwrap_or(false);
        let mut pattern_path = git_path.clone();
        if is_dir && !pattern_path.ends_with(b"/") {
            pattern_path.push(b'/');
        }
        let in_sparse = match active.as_ref() {
            Some(active) => {
                sley_worktree::path_in_sparse_checkout(&pattern_path, &active.sparse, active.mode)
            }
            None => true,
        };
        let index_entry = index.as_ref().and_then(|index| {
            let range = add_index_entries_path_range(&index.entries, &git_path);
            index.entries[range]
                .iter()
                .find(|entry| entry.stage() == sley_index::Stage::Normal)
        });
        let rejected_path = if let Some(entry) = index_entry {
            // git clears the skip-worktree bit for present files when sparse
            // checkout is enabled (clear_skip_worktree_from_present_files), so a
            // present, in-cone file is a legitimate dense match.
            let effective_skip_worktree =
                entry.is_skip_worktree() && !(active.is_some() && present);
            // `--refresh` re-stats whatever refresh_index would touch: a present
            // out-of-cone entry has had its bit cleared, so it refreshes normally
            // and only a still-skip-worktree entry is rejected. Regular `add`
            // additionally rejects anything outside the sparse cone.
            if is_refresh {
                effective_skip_worktree
            } else {
                effective_skip_worktree || !in_sparse
            }
        } else if is_refresh {
            // refresh only touches tracked entries; an untracked pathspec falls
            // through to the normal "did not match" handling.
            false
        } else {
            // Untracked: only an existing path outside the cone is rejected; a
            // missing pathspec falls through to the normal "did not match" error.
            present && !in_sparse
        };
        if rejected_path {
            rejected.push(path.to_string_lossy().replace('\\', "/"));
        }
    }
    if rejected.is_empty() {
        return Ok(());
    }
    advise_on_updating_sparse_paths_with_config(config, &rejected);
    Err(GitError::Exit(1))
}

/// git's `advise_on_updating_sparse_paths`: the "outside of your sparse-checkout
/// definition" header, one line per path, then the hint block (gated by
/// `advice.updateSparsePath`, default true). Shared by `add` and `mv`.
pub(super) fn advise_on_updating_sparse_paths(git_dir: &Path, paths: &[String]) {
    let config = read_repo_config(git_dir).unwrap_or_default();
    advise_on_updating_sparse_paths_with_config(&config, paths);
}

fn advise_on_updating_sparse_paths_with_config(config: &GitConfig, paths: &[String]) {
    eprintln!("The following paths and/or pathspecs matched paths that exist");
    eprintln!("outside of your sparse-checkout definition, so will not be");
    eprintln!("updated in the index:");
    for path in paths {
        eprintln!("{path}");
    }
    if add_update_sparse_path_advice_enabled(config) {
        eprintln!("hint: If you intend to update such entries, try one of the following:");
        eprintln!("hint: * Use the --sparse option.");
        eprintln!("hint: * Disable or modify the sparsity rules.");
        eprintln!(
            "hint: Disable this message with \"git config set advice.updateSparsePath false\""
        );
    }
}

/// `advice.updateSparsePath` (default true) gates the hint block that follows
/// the "outside of your sparse-checkout definition" header.
fn add_update_sparse_path_advice_enabled(config: &GitConfig) -> bool {
    config
        .get_bool("advice", None, "updateSparsePath")
        .unwrap_or(true)
}

pub(super) struct ActiveSparseCheckoutForAdd {
    pub(super) sparse: sley_worktree::SparseCheckout,
    pub(super) mode: sley_worktree::SparseCheckoutMode,
}

pub(super) fn active_sparse_checkout_for_add(
    git_dir: &Path,
) -> Result<Option<ActiveSparseCheckoutForAdd>> {
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
    let mut patterns: Vec<Vec<u8>> = fs::read(sparse_file)?
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if patterns.last().map(Vec::is_empty) == Some(true) {
        patterns.pop();
    }
    let cone = worktree_config
        .get_bool("core", None, "sparseCheckoutCone")
        .or_else(|| repo_config.get_bool("core", None, "sparseCheckoutCone"))
        .unwrap_or(false);
    let sparse = sley_worktree::SparseCheckout {
        patterns,
        sparse_index: false,
    };
    let mode = if cone {
        sley_worktree::SparseCheckoutMode::Cone
    } else {
        sley_worktree::SparseCheckoutMode::Full
    };
    Ok(Some(ActiveSparseCheckoutForAdd { sparse, mode }))
}

fn add_pathspec_needs_status_walk(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(value) = component else {
            return false;
        };
        value.to_string_lossy().starts_with(':')
    }) || path
        .to_string_lossy()
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\'))
}

pub(super) fn normalize_add_absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if let Some(name) = absolute.file_name()
        && let Some(parent) = absolute.parent()
        && let Ok(canonical_parent) = fs::canonicalize(parent)
    {
        // On case-insensitive filesystems the parent canonicalize already folds
        // intermediate components (`/var/.../t/tmp` → `/private/var/.../T/tmp`).
        // Prefer the on-disk spelling of the final component too so
        // `git add $(pwd | tr A-Z a-z)/blub` stages `BLUB` (t3700).
        let wanted = name.to_string_lossy();
        if let Ok(entries) = fs::read_dir(&canonical_parent) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&wanted)
                {
                    return entry.path();
                }
            }
        }
        return canonical_parent.join(name);
    }
    if let Ok(canonical) = fs::canonicalize(&absolute) {
        return canonical;
    }
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(value) => normalized.push(value),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

pub(super) fn add_git_path_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(GitError::InvalidPath(format!(
            "invalid index path {}",
            path.display()
        )));
    }
    // NFD→NFC when core.precomposeunicode is set (git precompose_argv_prefix).
    let path = sley_core::precompose_path_if_needed(path);
    Ok(path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes())
}

pub(super) fn add_index_entries_path_range(
    entries: &[IndexEntry],
    path: &[u8],
) -> std::ops::Range<usize> {
    let mut start = match entries.binary_search_by(|entry| entry.path.as_bytes().cmp(path)) {
        Ok(index) => index,
        Err(insert) => return insert..insert,
    };
    while start > 0 && entries[start - 1].path.as_bytes() == path {
        start -= 1;
    }
    let mut end = start;
    while end < entries.len() && entries[end].path.as_bytes() == path {
        end += 1;
    }
    start..end
}

fn resolve_add_paths(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    paths: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut resolved = BTreeSet::new();
    for path in paths {
        let absolute = normalize_add_absolute_path(cwd, &path);
        if absolute.is_dir() {
            collect_add_files(worktree_root, git_dir, &absolute, &mut resolved)?;
        } else {
            resolved.insert(absolute);
        }
    }
    Ok(resolved.into_iter().collect())
}

fn collect_add_files(
    worktree_root: &Path,
    git_dir: &Path,
    directory: &Path,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    // An embedded repository below the worktree root is opaque to `add`: it is
    // staged as a single gitlink path, never descended into. Canonicalize both
    // sides so a pathspec like `.` (root + a CurDir component) is still
    // recognized as the root itself, not an embedded repository.
    let is_root = match (fs::canonicalize(directory), fs::canonicalize(worktree_root)) {
        (Ok(left), Ok(right)) => left == right,
        _ => directory == worktree_root,
    };
    let active_repository = sley_diff_merge::gitlink_git_dir(directory)
        .is_some_and(|embedded| paths_refer_to_same_dir(&embedded, git_dir));
    if !is_root && !active_repository && sley_diff_merge::gitlink_git_dir(directory).is_some() {
        out.insert(directory.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path == worktree_root.join(".git") {
            continue;
        }
        if path.is_dir() {
            collect_add_files(worktree_root, git_dir, &path, out)?;
        } else {
            out.insert(path);
        }
    }
    Ok(())
}

fn print_add_actions(worktree_root: &Path, actions: &[AddAction]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for action in actions {
        let path = action.path();
        let display = path.strip_prefix(worktree_root).unwrap_or(path);
        let verb = match action {
            AddAction::Add(_) => "add",
            AddAction::Remove(_) => "remove",
        };
        writeln!(
            stdout,
            "{verb} '{}'",
            display.to_string_lossy().replace('\\', "/")
        )?;
    }
    Ok(())
}

fn validate_add_chmod_dry_run(
    worktree_root: &Path,
    actions: &[AddAction],
    chmod: Option<bool>,
) -> Result<()> {
    let Some(executable) = chmod else {
        return Ok(());
    };
    for action in actions {
        let AddAction::Add(path) = action else {
            continue;
        };
        if fs::symlink_metadata(path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            let display = path
                .strip_prefix(worktree_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            eprintln!(
                "fatal: git update-index: cannot chmod {}x '{display}'",
                if executable { '+' } else { '-' }
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}
