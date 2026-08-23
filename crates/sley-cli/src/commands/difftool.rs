use crate::commands::tool_launch::{
    ToolCommand, ToolEnvironment, ToolMode, config_bool, gui_default, print_tool_help,
    resolve_tool_command, run_tool_shell, select_tool_name,
};
use crate::*;

#[derive(Default)]
struct DifftoolOptions {
    cli_tool: Option<String>,
    extcmd: Option<String>,
    gui: Option<bool>,
    prompt: Option<bool>,
    dir_diff: bool,
    trust_exit_code: Option<bool>,
    symlinks: bool,
    cached: bool,
    rotate_to: Option<String>,
    skip_to: Option<String>,
    diff_args: Vec<String>,
}

pub(crate) fn cmd_difftool(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = parse_difftool_args(args)?;
    let lazy_fetch = cli_session.lazy_fetch();
    if options.extcmd.is_some() && (options.cli_tool.is_some() || options.gui == Some(true)) {
        return Err(GitError::Command(
            "difftool --gui, --tool and --extcmd are mutually exclusive".into(),
        ));
    }
    if options.cli_tool.is_some() && options.gui == Some(true) {
        return Err(GitError::Command(
            "difftool --gui, --tool and --extcmd are mutually exclusive".into(),
        ));
    }
    if options.diff_args.iter().any(|arg| arg == "--no-index") {
        return run_no_index_difftool(cli_session, &options);
    }

    let repo = RepositoryContext::from_session(cli_session)?;
    let git_dir = repo.git_dir();
    let worktree_root = repo.worktree_root()?;
    let config = repo.config();
    let gui = options
        .gui
        .unwrap_or_else(|| gui_default(config, ToolMode::Diff));
    let (diffs, right_side_is_worktree) = collect_difftool_entries(&repo, &options)?;
    let diffs = order_difftool_entries(
        diffs,
        options.rotate_to.as_deref(),
        options.skip_to.as_deref(),
    )?;
    if diffs.is_empty() {
        return Ok(());
    }
    let tool = resolve_difftool_tool(config, &options, gui)?;
    let prompt = should_prompt(config, &options);
    if options.dir_diff {
        return run_dir_difftool(&repo, &options, &tool, &diffs, lazy_fetch);
    }

    let temp = TempDir::new("sley-difftool")?;
    let total = diffs.len();
    for (idx, entry) in diffs.iter().enumerate() {
        let materialized = materialize_difftool_entry(
            repo.objects(),
            worktree_root,
            entry,
            &temp.path,
            lazy_fetch,
            right_side_is_worktree,
        )?;
        if prompt {
            let path = String::from_utf8_lossy(&entry.path);
            println!();
            println!("Viewing ({}/{}): '{}'", idx + 1, total, path);
            print!("Launch '{}' [Y/n]? ", display_tool_name(&options, &tool));
            io::stdout().flush()?;
            let mut ans = String::new();
            if io::stdin().read_line(&mut ans)? == 0 || ans.trim() == "n" {
                continue;
            }
        }
        let status = run_difftool_command(&options, &tool, entry, &materialized)?;
        if status >= 126 {
            return Err(GitError::Exit(128));
        }
        if status != 0 && tool.trust_exit_code {
            return Err(GitError::Exit(status));
        }
    }
    let _ = git_dir;
    Ok(())
}

fn parse_difftool_args(args: &[String]) -> Result<DifftoolOptions> {
    let mut options = DifftoolOptions::default();
    let mut i = 0;
    let mut passthrough = false;
    while i < args.len() {
        let arg = &args[i];
        if passthrough {
            options.diff_args.push(arg.clone());
            i += 1;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: git difftool [<options>] [<commit> [<commit>]] [--] [<path>...]");
                return Err(GitError::Exit(129));
            }
            "--tool-help" => {
                print_tool_help(ToolMode::Diff);
                return Err(GitError::Exit(0));
            }
            "--" => {
                passthrough = true;
                options.diff_args.push(arg.clone());
            }
            "-d" | "--dir-diff" => options.dir_diff = true,
            "--cached" | "--staged" => options.cached = true,
            "-g" | "--gui" => options.gui = Some(true),
            "--no-gui" => options.gui = Some(false),
            "-y" | "--no-prompt" => options.prompt = Some(false),
            "--prompt" => options.prompt = Some(true),
            "--trust-exit-code" => options.trust_exit_code = Some(true),
            "--no-trust-exit-code" => options.trust_exit_code = Some(false),
            "--symlinks" => options.symlinks = true,
            "--no-symlinks" => options.symlinks = false,
            "-t" | "--tool" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err(GitError::Command("--tool requires a value".into()));
                };
                options.cli_tool = Some(value.clone());
            }
            "-x" | "--extcmd" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err(GitError::Command("--extcmd requires a value".into()));
                };
                options.extcmd = Some(value.clone());
            }
            value if value.starts_with("--tool=") => {
                options.cli_tool = Some(value["--tool=".len()..].to_string());
            }
            value if value.starts_with("--extcmd=") => {
                options.extcmd = Some(value["--extcmd=".len()..].to_string());
            }
            value if value.starts_with("--rotate-to=") => {
                options.rotate_to = Some(value["--rotate-to=".len()..].to_string());
            }
            value if value.starts_with("--skip-to=") => {
                options.skip_to = Some(value["--skip-to=".len()..].to_string());
            }
            _ => options.diff_args.push(arg.clone()),
        }
        i += 1;
    }
    Ok(options)
}

fn resolve_difftool_tool(
    config: &GitConfig,
    options: &DifftoolOptions,
    gui: bool,
) -> Result<ToolCommand> {
    if let Some(extcmd) = &options.extcmd {
        return Ok(ToolCommand {
            name: extcmd.clone(),
            command: extcmd.clone(),
            trust_exit_code: options
                .trust_exit_code
                .unwrap_or_else(|| config_bool(config, "difftool", "trustexitcode", false)),
        });
    }
    let Some(tool) = select_tool_name(config, ToolMode::Diff, options.cli_tool.as_deref(), gui)
    else {
        eprintln!("No diff tool configured");
        return Err(GitError::Exit(1));
    };
    resolve_tool_command(config, ToolMode::Diff, &tool, options.trust_exit_code)
}

fn should_prompt(config: &GitConfig, options: &DifftoolOptions) -> bool {
    if let Some(prompt) = options.prompt {
        return prompt;
    }
    if env::var_os("GIT_DIFFTOOL_NO_PROMPT").is_some() {
        return false;
    }
    if env::var_os("GIT_DIFFTOOL_PROMPT").is_some() {
        return true;
    }
    config
        .get_bool("difftool", None, "prompt")
        .or_else(|| config.get_bool("mergetool", None, "prompt"))
        .unwrap_or(true)
}

/// Returns the diff entries plus whether the right-hand side is the live
/// worktree (`index-vs-worktree` / `tree-vs-worktree` modes), whose raw oids
/// are unresolved and must be read from disk.
fn collect_difftool_entries(
    repo: &RepositoryContext,
    options: &DifftoolOptions,
) -> Result<(Vec<sley_diff_merge::NameStatusEntry>, bool)> {
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();
    let worktree_root = repo.worktree_root()?;
    let (revs, paths) = split_difftool_revs(git_dir, format, db, &options.diff_args)?;
    let pathspec = if paths.is_empty() {
        DiffPathspec::default()
    } else {
        crate::diff_pathspec_new(cwd, worktree_root, &paths, repo.pathspec_magic())?
    };
    let base_options = sley_diff_merge::DiffNameStatusOptions::default();
    let mut entries = match (options.cached, revs.as_slice()) {
        (true, []) => sley_diff_merge::diff_name_status_head_index(git_dir, format)?,
        (true, [tree]) => sley_diff_merge::diff_name_status_tree_index_with_options(
            git_dir,
            format,
            tree,
            base_options,
        )?,
        (false, []) => {
            // Racy-clean files (stale stat info) must be re-classified through
            // the smudge/clean filters before being reported as modified,
            // matching commands::diff; without this, CRLF/ident/filter repos
            // phantom-show clean files as modified.
            let mut stat_clean_validator = sley_worktree::StatCleanFilterValidator::new();
            let mut validate_stat_clean =
                |entry: sley_diff_merge::IndexWorktreeValidationEntry<'_>,
                 absolute_path: &Path,
                 metadata: &fs::Metadata| {
                    stat_clean_validator.validate_path(
                        worktree_root,
                        git_dir,
                        format,
                        entry.mode,
                        entry.oid,
                        entry.size,
                        entry.path,
                        absolute_path,
                        metadata,
                    )
                };
            sley_diff_merge::diff_name_status_index_worktree_with_options_and_gitlinks_validated(
                worktree_root,
                git_dir,
                format,
                base_options,
                &mut validate_stat_clean,
            )?
            .entries
        }
        (false, [tree]) => sley_diff_merge::diff_name_status_tree_worktree_with_options(
            worktree_root,
            git_dir,
            format,
            tree,
            base_options,
        )?,
        (_, [left, right]) => sley_diff_merge::diff_name_status_trees_with_options(
            db,
            format,
            left,
            right,
            base_options,
        )?,
        _ => Vec::new(),
    };
    if options.cached {
        entries.retain(|entry| entry.status != sley_diff_merge::NameStatus::Unmerged);
    }
    let right_side_is_worktree = !options.cached && revs.len() <= 1;
    Ok((
        apply_diff_pathspec(entries, &pathspec),
        right_side_is_worktree,
    ))
}

fn split_difftool_revs(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    args: &[String],
) -> Result<(Vec<ObjectId>, Vec<String>)> {
    let mut revs = Vec::new();
    let mut paths = Vec::new();
    let mut after_sep = false;
    for arg in args {
        if after_sep {
            paths.push(arg.clone());
            continue;
        }
        if arg == "--" {
            after_sep = true;
            continue;
        }
        if revs.len() < 2
            && let Ok(oid) = sley_rev::RevisionResolver::new(git_dir, format, db).resolve(arg)
            && let Ok(tree) = sley_rev::peel_to_tree(db, format, &oid)
        {
            revs.push(tree);
            continue;
        }
        paths.push(arg.clone());
        after_sep = true;
    }
    Ok((revs, paths))
}

fn order_difftool_entries(
    mut entries: Vec<sley_diff_merge::NameStatusEntry>,
    rotate_to: Option<&str>,
    skip_to: Option<&str>,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    if let Some(path) = rotate_to {
        let Some(pos) = entries
            .iter()
            .position(|entry| String::from_utf8_lossy(&entry.path) == path)
        else {
            return Err(GitError::Exit(1));
        };
        entries.rotate_left(pos);
    }
    if let Some(path) = skip_to {
        let Some(pos) = entries
            .iter()
            .position(|entry| String::from_utf8_lossy(&entry.path) == path)
        else {
            return Err(GitError::Exit(1));
        };
        entries = entries.into_iter().skip(pos).collect();
    }
    Ok(entries)
}

fn materialize_difftool_entry(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    entry: &sley_diff_merge::NameStatusEntry,
    temp: &Path,
    lazy_fetch: bool,
    right_side_is_worktree: bool,
) -> Result<ToolEnvironment> {
    let rel = repo_path_to_path(&entry.path);
    let local = temp.join("left").join(&rel);
    let remote = temp.join("right").join(&rel);
    write_materialized(
        &local,
        diff_entry_old_content(entry, db, crate::diff_lazy_fetch(lazy_fetch))?.as_deref(),
        entry.old_mode,
    )?;
    write_materialized(
        &remote,
        diff_entry_new_content(
            entry,
            db,
            Some(worktree_root),
            // Worktree-involved comparisons report unresolved raw worktree
            // oids (the blob was never written), so the right side must be
            // read from the worktree file, not looked up in the odb.
            right_side_is_worktree,
            None,
            crate::diff_lazy_fetch(lazy_fetch),
        )?
        .as_deref(),
        entry.new_mode,
    )?;
    Ok(ToolEnvironment {
        local,
        remote,
        merged: worktree_root.join(&rel),
        base: worktree_root.join(&rel),
    })
}

fn write_materialized(path: &Path, content: Option<&[u8]>, mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content.unwrap_or_default())?;
    if mode == Some(0o120000) {
        return Ok(());
    }
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let executable = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(executable))?;
    }
    Ok(())
}

fn run_difftool_command(
    options: &DifftoolOptions,
    tool: &ToolCommand,
    entry: &sley_diff_merge::NameStatusEntry,
    envs: &ToolEnvironment,
) -> Result<i32> {
    if let Some(extcmd) = &options.extcmd {
        let merged_path = String::from_utf8_lossy(&entry.path);
        let merged = shell_quote(&merged_path);
        let local = shell_quote(&envs.local.to_string_lossy());
        let remote = shell_quote(&envs.remote.to_string_lossy());
        let extcmd = shell_quote(extcmd);
        let command = format!(
            "GIT_DIFFTOOL_EXTCMD={extcmd}; set -- {merged} {local} {remote}; eval $GIT_DIFFTOOL_EXTCMD '\"$LOCAL\"' '\"$REMOTE\"'"
        );
        return run_tool_shell(&command, envs);
    }
    let mut envs = envs.clone();
    if tool.command.contains("$BASE") {
        envs.base = envs.merged.clone();
    }
    run_tool_shell(&tool.command, &envs)
}

fn run_dir_difftool(
    repo: &RepositoryContext,
    options: &DifftoolOptions,
    tool: &ToolCommand,
    entries: &[sley_diff_merge::NameStatusEntry],
    lazy_fetch: bool,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut temp = TempDir::new("sley-difftool-dir")?;
    let left = temp.path.join("left");
    let right = temp.path.join("right");
    fs::create_dir_all(&left)?;
    fs::create_dir_all(&right)?;
    let mut snapshots = Vec::new();
    for entry in entries {
        let rel = repo_path_to_path(&entry.path);
        write_dir_materialized(
            &left.join(&rel),
            diff_entry_old_content(entry, repo.objects(), crate::diff_lazy_fetch(lazy_fetch))?.as_deref(),
            entry.old_mode,
        )?;
        let right_path = right.join(&rel);
        let mut right_was_symlink = false;
        if options.symlinks && can_symlink_right_side(repo, entry, lazy_fetch)? {
            symlink_worktree_file(repo.worktree_root()?.join(&rel), &right_path)?;
            right_was_symlink = true;
        } else {
            write_dir_materialized(
                &right_path,
                dir_diff_new_content(repo, entry, lazy_fetch)?.as_deref(),
                entry.new_mode,
            )?;
        }
        let worktree_path = repo.worktree_root()?.join(&rel);
        snapshots.push(DirDiffSnapshot {
            rel,
            right: (!right_was_symlink)
                .then(|| fs::read(&right_path).ok())
                .flatten(),
            worktree: fs::read(worktree_path).ok(),
            right_was_symlink,
        });
    }
    let envs = ToolEnvironment {
        local: left.clone(),
        remote: right.clone(),
        merged: repo.worktree_root()?.to_path_buf(),
        base: repo.worktree_root()?.to_path_buf(),
    };
    let status = if let Some(extcmd) = &options.extcmd {
        let command = format!(
            "{} {} {}",
            shell_quote(extcmd),
            shell_quote(&left.to_string_lossy()),
            shell_quote(&right.to_string_lossy())
        );
        run_tool_shell_in_dir(&command, &envs, repo.cwd())?
    } else {
        run_tool_shell_in_dir(&tool.command, &envs, repo.cwd())?
    };
    if status >= 126 || (status != 0 && tool.trust_exit_code) {
        return Err(GitError::Exit(status));
    }
    let mut conflict = false;
    for snapshot in &snapshots {
        let changed = right.join(&snapshot.rel);
        if snapshot.right_was_symlink {
            continue;
        }
        if changed.is_file() {
            let changed_content = fs::read(&changed)?;
            if snapshot.right.as_ref() == Some(&changed_content) {
                continue;
            }
            let wt = repo.worktree_root()?.join(&snapshot.rel);
            if fs::read(&wt).ok() != snapshot.worktree {
                eprintln!(
                    "warning: both files modified: '{}' and '{}'.",
                    wt.display(),
                    changed.display()
                );
                eprintln!("warning: working tree file has been left.");
                eprintln!("warning: ");
                conflict = true;
                continue;
            }
            if let Some(parent) = wt.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(changed, wt)?;
        }
    }
    if conflict {
        eprintln!(
            "warning: temporary files exist in '{}'.",
            temp.path.display()
        );
        eprintln!("warning: you may want to cleanup or recover these.");
        temp.keep();
        return Err(GitError::Exit(1));
    }
    Ok(())
}

struct DirDiffSnapshot {
    rel: PathBuf,
    right: Option<Vec<u8>>,
    worktree: Option<Vec<u8>>,
    right_was_symlink: bool,
}

fn write_dir_materialized(path: &Path, content: Option<&[u8]>, mode: Option<u32>) -> Result<()> {
    let Some(content) = content else {
        return Ok(());
    };
    write_materialized(path, Some(content), mode)
}

fn dir_diff_new_content(
    repo: &RepositoryContext,
    entry: &sley_diff_merge::NameStatusEntry,
    lazy_fetch: bool,
) -> Result<Option<Vec<u8>>> {
    if entry.new_mode == Some(0o120000) && entry.new_oid.is_none() {
        let path = repo.worktree_root()?.join(repo_path_to_path(&entry.path));
        return Ok(Some(
            fs::read_link(path)?.to_string_lossy().as_bytes().to_vec(),
        ));
    }
    diff_entry_new_content(
        entry,
        repo.objects(),
        Some(repo.worktree_root()?),
        entry.new_oid.is_none(),
        None,
        crate::diff_lazy_fetch(lazy_fetch),
    )
}

fn can_symlink_right_side(
    repo: &RepositoryContext,
    entry: &sley_diff_merge::NameStatusEntry,
    lazy_fetch: bool,
) -> Result<bool> {
    if !is_regular_file_mode(entry.new_mode) {
        return Ok(false);
    }
    let Some(oid) = entry.new_oid.as_ref() else {
        return Ok(false);
    };
    let worktree_path = repo.worktree_root()?.join(repo_path_to_path(&entry.path));
    let Ok(metadata) = fs::symlink_metadata(&worktree_path) else {
        return Ok(false);
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(false);
    }
    Ok(read_blob(repo.objects(), oid, crate::diff_lazy_fetch(lazy_fetch))? == fs::read(worktree_path)?)
}

fn is_regular_file_mode(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170000 == 0o100000)
}

#[cfg(unix)]
fn symlink_worktree_file(target: PathBuf, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(link);
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

fn run_tool_shell_in_dir(command: &str, envs: &ToolEnvironment, cwd: &Path) -> Result<i32> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("LOCAL", &envs.local)
        .env("REMOTE", &envs.remote)
        .env("MERGED", &envs.merged)
        .env("BASE", &envs.base)
        .status()
        .map_err(|err| GitError::Command(format!("failed to run tool: {err}")))?;
    Ok(status.code().unwrap_or(128))
}

#[cfg(not(unix))]
fn symlink_worktree_file(target: PathBuf, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(target, link)?;
    Ok(())
}

fn run_no_index_difftool(
    cli_session: &crate::session::CliSession,
    options: &DifftoolOptions,
) -> Result<()> {
    let paths: Vec<&String> = options
        .diff_args
        .iter()
        .filter(|arg| arg.as_str() != "--no-index")
        .collect();
    if paths.len() < 2 {
        return Err(GitError::Exit(129));
    }
    let config = load_no_index_difftool_config(cli_session)?;
    let tool = resolve_difftool_tool(&config, options, false)?;
    let envs = ToolEnvironment {
        local: PathBuf::from(paths[0]),
        remote: PathBuf::from(paths[1]),
        merged: PathBuf::from(paths[0]),
        base: PathBuf::from(paths[0]),
    };
    let status = run_difftool_command(
        options,
        &tool,
        &sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Modified,
            path: paths[0].as_bytes().to_vec().into(),
            old_path: None,
            old_mode: Some(0o100644),
            new_mode: Some(0o100644),
            old_oid: None,
            new_oid: None,
        },
        &envs,
    )?;
    if status == 0 {
        Err(GitError::Exit(1))
    } else {
        Err(GitError::Exit(status))
    }
}

fn load_no_index_difftool_config(cli_session: &crate::session::CliSession) -> Result<GitConfig> {
    if let Ok(repo) = RepositoryContext::from_session(cli_session) {
        return Ok(repo.config().clone());
    }

    let context = sley_config::ConfigIncludeContext::default();
    let mut config = sley_config::load_pre_dispatch_config(None, &context)?;
    if let Ok(parameters) =
        sley_config::injected_config_parameters(effective_config_parameters_env().as_deref())
    {
        let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            &base,
        )?;
    }
    Ok(config)
}

fn display_tool_name<'a>(options: &'a DifftoolOptions, tool: &'a ToolCommand) -> &'a str {
    options.extcmd.as_deref().unwrap_or(&tool.name)
}

fn shell_quote(value: &str) -> String {
    let mut out = String::from("'");
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

struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl TempDir {
    fn new(prefix: &str) -> Result<Self> {
        let mut root = env::temp_dir().to_string_lossy().into_owned();
        while root.len() > 1 && root.ends_with(std::path::MAIN_SEPARATOR) {
            root.pop();
        }
        let mut path = PathBuf::from(root);
        let unique = format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        );
        path.push(unique);
        fs::create_dir_all(&path)?;
        Ok(Self { path, keep: false })
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
