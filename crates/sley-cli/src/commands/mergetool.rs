use crate::commands::tool_launch::{
    ToolCommand, ToolEnvironment, ToolMode, gui_default, print_tool_help, resolve_tool_command,
    run_tool_shell_in_dir, select_tool_name,
};
use crate::*;

#[derive(Default)]
struct MergetoolOptions {
    cli_tool: Option<String>,
    gui: Option<bool>,
    prompt: Option<bool>,
    order_file: Option<String>,
    paths: Vec<String>,
}

#[derive(Clone)]
struct UnmergedPath {
    path: Vec<u8>,
    base: Option<IndexEntry>,
    local: Option<IndexEntry>,
    remote: Option<IndexEntry>,
}

pub(crate) fn cmd_mergetool(args: &[String]) -> Result<()> {
    let options = parse_mergetool_args(args)?;
    let repo = RepositoryContext::discover_current()?;
    let config = repo.config();
    let gui = options
        .gui
        .unwrap_or_else(|| gui_default(config, ToolMode::Merge));
    let Some(tool_name) =
        select_tool_name(config, ToolMode::Merge, options.cli_tool.as_deref(), gui)
    else {
        eprintln!("No merge tool configured");
        return Err(GitError::Exit(1));
    };
    let tool = resolve_tool_command(config, ToolMode::Merge, &tool_name, None)?;
    let mut conflicts = collect_unmerged_paths(repo.git_dir(), repo.format())?;
    conflicts = filter_mergetool_paths(&repo, conflicts, &options)?;
    order_mergetool_paths(&mut conflicts, repo.worktree_root()?, config, &options)?;
    if conflicts.is_empty() {
        println!("No files need merging");
        return Ok(());
    }
    println!("Merging:");
    for conflict in &conflicts {
        println!("{}", String::from_utf8_lossy(&conflict.path));
    }

    let mut failed = false;
    for conflict in conflicts {
        if !run_one_mergetool_path(&repo, config, &options, &tool, &conflict)? {
            failed = true;
        }
    }
    if failed {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

fn parse_mergetool_args(args: &[String]) -> Result<MergetoolOptions> {
    let mut options = MergetoolOptions::default();
    let mut i = 0;
    let mut passthrough = false;
    while i < args.len() {
        let arg = &args[i];
        if passthrough {
            options.paths.push(arg.clone());
            i += 1;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: git mergetool [--tool=tool] [file to merge] ...");
                return Err(GitError::Exit(129));
            }
            "--tool-help" => {
                print_tool_help(ToolMode::Merge);
                return Ok(options);
            }
            "--" => passthrough = true,
            "-t" | "--tool" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err(GitError::Command("--tool requires a value".into()));
                };
                options.cli_tool = Some(value.clone());
            }
            "-g" | "--gui" => options.gui = Some(true),
            "--no-gui" => options.gui = Some(false),
            "-y" | "--no-prompt" => options.prompt = Some(false),
            "--prompt" => options.prompt = Some(true),
            value if value.starts_with("--tool=") => {
                options.cli_tool = Some(value["--tool=".len()..].to_string());
            }
            value if value.starts_with("-O") && value.len() > 2 => {
                options.order_file = Some(value[2..].to_string());
            }
            "-O" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err(GitError::Command("-O requires a value".into()));
                };
                options.order_file = Some(value.clone());
            }
            _ => options.paths.push(arg.clone()),
        }
        i += 1;
    }
    Ok(options)
}

fn collect_unmerged_paths(git_dir: &Path, format: ObjectFormat) -> Result<Vec<UnmergedPath>> {
    let index = sley_worktree::read_repository_index(git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });
    let mut map: BTreeMap<Vec<u8>, UnmergedPath> = BTreeMap::new();
    for entry in index.entries {
        let stage = index_entry_stage(&entry);
        if stage == 0 {
            continue;
        }
        let path = entry.path.clone().into_bytes();
        let slot = map.entry(path.clone()).or_insert(UnmergedPath {
            path,
            base: None,
            local: None,
            remote: None,
        });
        match stage {
            1 => slot.base = Some(entry),
            2 => slot.local = Some(entry),
            3 => slot.remote = Some(entry),
            _ => {}
        }
    }
    Ok(map.into_values().collect())
}

fn filter_mergetool_paths(
    repo: &RepositoryContext,
    conflicts: Vec<UnmergedPath>,
    options: &MergetoolOptions,
) -> Result<Vec<UnmergedPath>> {
    if options.paths.is_empty() {
        return Ok(conflicts);
    }
    let filters = options
        .paths
        .iter()
        .map(|path| normalize_user_path(repo.cwd(), repo.worktree_root()?, path))
        .collect::<Result<Vec<_>>>()?;
    Ok(conflicts
        .into_iter()
        .filter(|conflict| {
            filters.iter().any(|filter| {
                conflict.path == *filter
                    || (conflict.path.starts_with(filter)
                        && filter.last().is_none_or(|last| *last == b'/'))
                    || conflict.path.starts_with(&with_trailing_slash(filter))
            })
        })
        .collect())
}

fn order_mergetool_paths(
    conflicts: &mut Vec<UnmergedPath>,
    worktree_root: &Path,
    config: &GitConfig,
    options: &MergetoolOptions,
) -> Result<()> {
    let order_file = options
        .order_file
        .as_deref()
        .or_else(|| config.get("diff", None, "orderfile"));
    let Some(order_file) = order_file else {
        return Ok(());
    };
    if order_file == "/dev/null" {
        return Ok(());
    }
    let path = if Path::new(order_file).is_absolute() {
        PathBuf::from(order_file)
    } else {
        worktree_root.join(order_file)
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(());
    };
    let order = contents
        .lines()
        .enumerate()
        .map(|(idx, line)| (line.as_bytes().to_vec(), idx))
        .collect::<HashMap<_, _>>();
    conflicts.sort_by(|left, right| {
        let l = order.get(&left.path).copied().unwrap_or(usize::MAX);
        let r = order.get(&right.path).copied().unwrap_or(usize::MAX);
        l.cmp(&r).then_with(|| left.path.cmp(&right.path))
    });
    Ok(())
}

fn run_one_mergetool_path(
    repo: &RepositoryContext,
    config: &GitConfig,
    options: &MergetoolOptions,
    tool: &ToolCommand,
    conflict: &UnmergedPath,
) -> Result<bool> {
    let worktree_root = repo.worktree_root()?;
    let merged = worktree_root.join(repo_path_to_path(&conflict.path));
    if is_delete_conflict(conflict) {
        return resolve_delete_conflict(repo, config, conflict, &merged);
    }
    if is_gitlink_conflict(conflict) {
        return resolve_gitlink_conflict(repo, conflict, &merged);
    }

    let materialized = materialize_mergetool_files(repo, config, conflict, &merged)?;
    if should_prompt(config, options) {
        print!(
            "Hit return to start merge resolution tool ({}) for '{}': ",
            tool.name,
            String::from_utf8_lossy(&conflict.path)
        );
        io::stdout().flush()?;
        let mut ignored = String::new();
        if io::stdin().read_line(&mut ignored)? == 0 {
            return Ok(false);
        }
    }
    let status = run_tool_shell_in_dir(&tool.command, &materialized, worktree_root)?;
    if status != 0 && tool.trust_exit_code {
        cleanup_mergetool_files(config, worktree_root, &materialized, &merged, false)?;
        return Ok(false);
    }
    stage_worktree_path(repo, &conflict.path)?;
    cleanup_mergetool_files(config, worktree_root, &materialized, &merged, true)?;
    Ok(true)
}

fn materialize_mergetool_files(
    repo: &RepositoryContext,
    config: &GitConfig,
    conflict: &UnmergedPath,
    merged: &Path,
) -> Result<ToolEnvironment> {
    let write_to_temp = config
        .get_bool("mergetool", None, "writetotemp")
        .unwrap_or(false);
    let base_dir = if write_to_temp {
        let dir = make_temp_dir("sley-mergetool")?;
        Some(dir)
    } else {
        None
    };
    let stem = merged
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("MERGED");
    let parent = if let Some(dir) = &base_dir {
        dir.as_path()
    } else {
        merged.parent().unwrap_or_else(|| Path::new("."))
    };
    fs::create_dir_all(parent)?;
    let local = parent.join(format!("{stem}_LOCAL_{}", std::process::id()));
    let remote = parent.join(format!("{stem}_REMOTE_{}", std::process::id()));
    let base = parent.join(format!("{stem}_BASE_{}", std::process::id()));
    write_stage_file(repo.objects(), conflict.local.as_ref(), &local)?;
    write_stage_file(repo.objects(), conflict.remote.as_ref(), &remote)?;
    write_stage_file(repo.objects(), conflict.base.as_ref(), &base)?;
    let local = display_mergetool_temp_path(&local, repo.worktree_root()?, write_to_temp);
    let remote = display_mergetool_temp_path(&remote, repo.worktree_root()?, write_to_temp);
    let base = display_mergetool_temp_path(&base, repo.worktree_root()?, write_to_temp);
    Ok(ToolEnvironment {
        local,
        remote,
        merged: merged.to_path_buf(),
        base,
    })
}

fn write_stage_file(
    db: &FileObjectDatabase,
    entry: Option<&IndexEntry>,
    path: &Path,
) -> Result<()> {
    let content = match entry {
        Some(entry) if entry.mode == 0o160000 => entry.oid.to_string().into_bytes(),
        Some(entry) => read_blob(db, &entry.oid)?,
        None => Vec::new(),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn cleanup_mergetool_files(
    config: &GitConfig,
    worktree_root: &Path,
    envs: &ToolEnvironment,
    merged: &Path,
    success: bool,
) -> Result<()> {
    let keep_temporaries = config
        .get_bool("mergetool", None, "keeptemporaries")
        .unwrap_or(false);
    let keep_backup = config
        .get_bool("mergetool", None, "keepbackup")
        .unwrap_or(true);
    if success && keep_backup && merged.exists() {
        let backup = merged.with_extension(format!(
            "{}orig",
            merged
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|_| "")
                .unwrap_or("")
        ));
        let _ = fs::copy(merged, backup);
    }
    if !keep_temporaries {
        let _ = fs::remove_file(resolve_mergetool_env_path(worktree_root, &envs.local));
        let _ = fs::remove_file(resolve_mergetool_env_path(worktree_root, &envs.remote));
        let _ = fs::remove_file(resolve_mergetool_env_path(worktree_root, &envs.base));
    }
    Ok(())
}

fn resolve_delete_conflict(
    repo: &RepositoryContext,
    config: &GitConfig,
    conflict: &UnmergedPath,
    merged: &Path,
) -> Result<bool> {
    loop {
        if conflict.base.is_some() {
            print!("Use (m)odified or (d)eleted file, or (a)bort? ");
        } else {
            print!("Use (c)reated or (d)eleted file, or (a)bort? ");
        }
        io::stdout().flush()?;
        let mut ans = String::new();
        if io::stdin().read_line(&mut ans)? == 0 {
            return Ok(false);
        }
        match ans.chars().next().unwrap_or('\n').to_ascii_lowercase() {
            'm' | 'c' => {
                stage_worktree_path(repo, &conflict.path)?;
                return Ok(true);
            }
            'd' => {
                let _ = fs::remove_file(merged);
                remove_index_path(repo, &conflict.path)?;
                let _ = config;
                return Ok(true);
            }
            'a' => return Ok(false),
            _ => {}
        }
    }
}

fn resolve_gitlink_conflict(
    repo: &RepositoryContext,
    conflict: &UnmergedPath,
    merged: &Path,
) -> Result<bool> {
    loop {
        print!("Use (l)ocal or (r)emote, or (a)bort? ");
        io::stdout().flush()?;
        let mut ans = String::new();
        if io::stdin().read_line(&mut ans)? == 0 {
            return Ok(false);
        }
        match ans.chars().next().unwrap_or('\n').to_ascii_lowercase() {
            'l' => {
                if let Some(entry) = &conflict.local {
                    stage_cacheinfo(repo, &conflict.path, entry.mode, entry.oid)?;
                } else {
                    let _ = fs::remove_dir_all(merged);
                    remove_index_path(repo, &conflict.path)?;
                }
                return Ok(true);
            }
            'r' => {
                if let Some(entry) = &conflict.remote {
                    stage_cacheinfo(repo, &conflict.path, entry.mode, entry.oid)?;
                } else {
                    let _ = fs::remove_dir_all(merged);
                    remove_index_path(repo, &conflict.path)?;
                }
                return Ok(true);
            }
            'a' => return Ok(false),
            _ => {}
        }
    }
}

fn stage_worktree_path(repo: &RepositoryContext, path: &[u8]) -> Result<()> {
    let config = read_repo_config(repo.git_dir())?;
    sley_worktree::update_index_ordered_paths_filtered(
        repo.worktree_root()?,
        repo.git_dir().to_path_buf(),
        repo.format(),
        &[sley_worktree::UpdateIndexPath {
            path: repo_path_to_path(path),
            mode: sley_worktree::UpdateIndexPathMode {
                add: true,
                ..Default::default()
            },
        }],
        sley_worktree::UpdateIndexOptions {
            add: true,
            remove: false,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
            allow_skip_worktree_entries: false,
        },
        &config,
        false,
    )?;
    Ok(())
}

fn stage_cacheinfo(repo: &RepositoryContext, path: &[u8], mode: u32, oid: ObjectId) -> Result<()> {
    sley_worktree::update_index_index_info(
        repo.git_dir(),
        repo.format(),
        &[
            sley_worktree::IndexInfoRecord::Remove {
                path: path.to_vec(),
            },
            sley_worktree::IndexInfoRecord::Add(sley_worktree::CacheInfoEntry {
                mode,
                oid,
                path: path.to_vec(),
                stage: 0,
            }),
        ],
    )?;
    Ok(())
}

fn remove_index_path(repo: &RepositoryContext, path: &[u8]) -> Result<()> {
    sley_worktree::update_index_index_info(
        repo.git_dir(),
        repo.format(),
        &[sley_worktree::IndexInfoRecord::Remove {
            path: path.to_vec(),
        }],
    )?;
    Ok(())
}

fn is_delete_conflict(conflict: &UnmergedPath) -> bool {
    conflict.local.is_none() || conflict.remote.is_none()
}

fn is_gitlink_conflict(conflict: &UnmergedPath) -> bool {
    conflict
        .local
        .as_ref()
        .is_some_and(|entry| entry.mode == 0o160000)
        || conflict
            .remote
            .as_ref()
            .is_some_and(|entry| entry.mode == 0o160000)
}

fn should_prompt(config: &GitConfig, options: &MergetoolOptions) -> bool {
    options
        .prompt
        .unwrap_or_else(|| config.get_bool("mergetool", None, "prompt").unwrap_or(true))
}

fn normalize_user_path(cwd: &Path, worktree_root: &Path, value: &str) -> Result<Vec<u8>> {
    let path = Path::new(value);
    let absolute = lexical_normalize_path(&if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    });
    let relative = absolute
        .strip_prefix(worktree_root)
        .unwrap_or(path)
        .to_string_lossy()
        .trim_start_matches("./")
        .replace('\\', "/");
    Ok(relative.into_bytes())
}

fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

fn with_trailing_slash(path: &[u8]) -> Vec<u8> {
    let mut out = path.to_vec();
    if !out.ends_with(b"/") {
        out.push(b'/');
    }
    out
}

fn make_temp_dir(prefix: &str) -> Result<PathBuf> {
    let mut path = env::temp_dir();
    path.push(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn display_mergetool_temp_path(path: &Path, worktree_root: &Path, write_to_temp: bool) -> PathBuf {
    if write_to_temp {
        return path.to_path_buf();
    }
    match path.strip_prefix(worktree_root) {
        Ok(relative) => PathBuf::from(".").join(relative),
        Err(_) => path.to_path_buf(),
    }
}

fn resolve_mergetool_env_path(worktree_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match path.strip_prefix(".") {
        Ok(relative) => worktree_root.join(relative),
        Err(_) => worktree_root.join(path),
    }
}
