//! Native implementation of Scalar's auxiliary command surface.
//!
//! Scalar remains a separate executable, but repository work is dispatched
//! in-process through Sley. This module owns only Scalar-specific enlistment
//! discovery and argument translation.

use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use sley::plumbing::sley_config::{ConfigEntry, ConfigSection, GitConfig};
#[cfg(unix)]
use sley::plumbing::sley_worktree::{FsmonitorDaemonSession, FsmonitorDaemonState};
use sley::{GitError, Repository, Result};

const USAGE: &str = "usage: scalar [-C <directory>] [-c <key>=<value>] <command> [<options>]";
const CLONE_USAGE: &str = "usage: scalar clone [--single-branch] [--branch <main-branch>] [--full-clone]\n\t[--[no-]src] [--[no-]tags] [--[no-]maintenance] <url> [<enlistment>]";

const RECOMMENDED_CONFIG: &[(&str, &str)] = &[
    ("am.keepCR", "true"),
    ("commitGraph.changedPaths", "true"),
    ("commitGraph.generationVersion", "1"),
    ("core.autoCRLF", "false"),
    ("core.safeCRLF", "false"),
    #[cfg(unix)]
    ("core.fsmonitor", "true"),
    ("credential.https://dev.azure.com.useHttpPath", "true"),
    ("feature.experimental", "false"),
    ("feature.manyFiles", "false"),
    ("fetch.showForcedUpdates", "false"),
    ("fetch.unpackLimit", "1"),
    ("fetch.writeCommitGraph", "false"),
    ("gc.auto", "0"),
    ("gui.GCWarning", "false"),
    ("index.skipHash", "true"),
    ("index.threads", "true"),
    ("index.version", "4"),
    ("merge.renames", "true"),
    ("merge.stat", "false"),
    ("pack.useBitmaps", "false"),
    ("pack.usePathWalk", "true"),
    ("receive.autoGC", "false"),
    ("status.aheadBehind", "false"),
    ("core.untrackedCache", "true"),
    ("log.excludeDecoration", "refs/prefetch/*"),
];

/// Run the native Scalar auxiliary command.
pub fn run_scalar(args: Vec<String>) -> Result<()> {
    let invocation = parse_global_args(&args)?;
    let Some(command) = invocation.command else {
        return usage();
    };
    match command {
        "clone" => clone(invocation.rest, &invocation.base),
        "delete" => delete(invocation.rest, &invocation.base),
        "diagnose" => diagnose(invocation.rest, &invocation.base),
        "list" => list(invocation.rest),
        "reconfigure" => reconfigure(
            invocation.rest,
            &invocation.config_overrides,
            &invocation.base,
        ),
        "register" => register(
            invocation.rest,
            &invocation.config_overrides,
            &invocation.base,
        ),
        "run" => run(invocation.rest, &invocation.base),
        "unregister" => unregister(invocation.rest, &invocation.base),
        "-h" | "--help" => usage(),
        _ => usage(),
    }
}

struct ScalarInvocation<'a> {
    command: Option<&'a str>,
    rest: &'a [String],
    config_overrides: Vec<String>,
    base: PathBuf,
}

fn parse_global_args(args: &[String]) -> Result<ScalarInvocation<'_>> {
    parse_global_args_from(args, env::current_dir()?)
}

fn parse_global_args_from(args: &[String], mut base: PathBuf) -> Result<ScalarInvocation<'_>> {
    let mut index = 0;
    let mut config_overrides = Vec::new();
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-C" => {
                index += 1;
                let Some(directory) = args.get(index) else {
                    return usage();
                };
                if directory.is_empty() {
                    index += 1;
                    continue;
                }
                let requested = absolutize(&base, Path::new(directory));
                base = fs::canonicalize(&requested).map_err(|_| {
                    eprintln!(
                        "fatal: cannot change to '{}': No such file or directory",
                        directory
                    );
                    GitError::Exit(128)
                })?;
            }
            "-c" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return usage();
                };
                config_overrides.push(value.clone());
            }
            value if value.starts_with("-c=") => {
                config_overrides.push(value[3..].to_string());
            }
            _ => {
                return Ok(ScalarInvocation {
                    command: Some(arg),
                    rest: &args[index + 1..],
                    config_overrides,
                    base,
                });
            }
        }
        index += 1;
    }
    Ok(ScalarInvocation {
        command: None,
        rest: &[],
        config_overrides,
        base,
    })
}

#[derive(Debug)]
struct Enlistment {
    worktree: PathBuf,
    root: PathBuf,
}

fn register(args: &[String], config_overrides: &[String], base: &Path) -> Result<()> {
    let (path, maintenance) = parse_register_args(args)?;
    let enlistment = resolve_enlistment(base, path.as_deref())?;
    configure_repository(&enlistment.worktree, config_overrides)?;
    add_global_value("scalar", "repo", &path_text(&enlistment.worktree))?;
    if maintenance {
        start_maintenance(&enlistment.worktree);
    }
    start_fsmonitor(&enlistment.worktree)?;
    Ok(())
}

fn parse_register_args(args: &[String]) -> Result<(Option<PathBuf>, bool)> {
    let mut maintenance = true;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "--maintenance" => maintenance = true,
            "--no-maintenance" => maintenance = false,
            value if value.starts_with('-') => return usage(),
            value if path.is_none() => path = Some(PathBuf::from(value)),
            _ => return usage(),
        }
    }
    Ok((path, maintenance))
}

fn list(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return usage();
    }
    for repository in global_values("scalar", "repo")? {
        println!("{repository}");
    }
    Ok(())
}

fn unregister(args: &[String], base: &Path) -> Result<()> {
    if args.len() > 1 || args.first().is_some_and(|arg| arg.starts_with('-')) {
        return usage();
    }
    let requested = args.first().map(PathBuf::from);
    let registered = matching_registered_worktree(base, requested.as_deref())?;
    if let Some(worktree) = registered {
        unregister_worktree(Path::new(&worktree))?;
    }
    Ok(())
}

fn delete(args: &[String], base: &Path) -> Result<()> {
    if args.len() != 1 || args[0].starts_with('-') {
        return usage();
    }
    let requested = absolutize(base, Path::new(&args[0]));
    let enlistment = resolve_enlistment(base, Some(&requested))?;
    if base.starts_with(&enlistment.root) {
        eprintln!("error: refusing to delete current working directory");
        return Err(GitError::Exit(1));
    }
    unregister_worktree(&enlistment.worktree)?;
    fs::remove_dir_all(enlistment.root)?;
    Ok(())
}

fn unregister_worktree(worktree: &Path) -> Result<()> {
    stop_fsmonitor(worktree)?;
    let value = path_text(worktree);
    remove_global_value("scalar", "repo", &value)?;
    remove_global_value("maintenance", "repo", &value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaintenanceMode {
    Enable,
    Disable,
    Keep,
}

fn reconfigure(args: &[String], config_overrides: &[String], base: &Path) -> Result<()> {
    let mut all = false;
    let mut maintenance = MaintenanceMode::Enable;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "-a" | "--all" => all = true,
            "--maintenance=enable" => maintenance = MaintenanceMode::Enable,
            "--maintenance=disable" => maintenance = MaintenanceMode::Disable,
            "--maintenance=keep" => maintenance = MaintenanceMode::Keep,
            value if value.starts_with('-') => return usage(),
            value if path.is_none() => path = Some(PathBuf::from(value)),
            _ => return usage(),
        }
    }
    if all && path.is_some() {
        return usage();
    }

    let worktrees = if all {
        global_values("scalar", "repo")?
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    } else {
        vec![resolve_enlistment(base, path.as_deref())?.worktree]
    };
    for worktree in worktrees {
        if !worktree.is_dir() || !is_worktree_repository(&worktree) {
            let value = path_text(&worktree);
            remove_global_value("scalar", "repo", &value)?;
            remove_global_value("maintenance", "repo", &value)?;
            continue;
        }
        configure_repository(&worktree, config_overrides)?;
        match maintenance {
            MaintenanceMode::Enable => start_maintenance(&worktree),
            MaintenanceMode::Disable => stop_maintenance(&worktree)?,
            MaintenanceMode::Keep => {}
        }
        start_fsmonitor(&worktree)?;
    }
    Ok(())
}

fn run(args: &[String], base: &Path) -> Result<()> {
    if args.len() != 2 || args.iter().any(|arg| arg.starts_with('-')) {
        return usage();
    }
    let requested = absolutize(base, Path::new(&args[1]));
    if !requested.is_dir() {
        eprintln!("fatal: '{}' does not exist", args[1]);
        return Err(GitError::Exit(128));
    }
    let enlistment = resolve_enlistment(base, Some(&requested))?;
    crate::run(vec![
        "-C".into(),
        path_text(&enlistment.worktree),
        "maintenance".into(),
        "run".into(),
        format!("--task={}", args[0]),
    ])
}

fn resolve_enlistment(base: &Path, requested: Option<&Path>) -> Result<Enlistment> {
    let requested = requested
        .map(|path| absolutize(base, path))
        .unwrap_or_else(|| base.to_path_buf());
    if !requested.is_dir() {
        eprintln!("fatal: '{}' does not exist", requested.display());
        return Err(GitError::Exit(128));
    }

    let src = requested.join("src");
    let has_src_repository = is_worktree_repository(&src);
    let repository = if has_src_repository {
        Repository::open_from_environment(&src)?
    } else {
        Repository::open_from_environment(&requested).map_err(|_| {
            eprintln!("fatal: not a git repository (or any of the parent directories): .git");
            GitError::Exit(128)
        })?
    };
    let Some(worktree) = repository.workdir() else {
        return Err(scalar_requires_worktree());
    };
    if paths_equal(&requested, repository.git_dir()) {
        return Err(scalar_requires_worktree());
    }
    if !has_src_repository && discovery_blocked_by_ceiling(&requested, &worktree) {
        eprintln!("fatal: not a git repository (or any of the parent directories): .git");
        return Err(GitError::Exit(128));
    }
    let worktree = fs::canonicalize(worktree)?;
    let root = if has_src_repository {
        fs::canonicalize(&requested)?
    } else {
        worktree.clone()
    };
    Ok(Enlistment { worktree, root })
}

fn matching_registered_worktree(base: &Path, requested: Option<&Path>) -> Result<Option<String>> {
    let requested = requested
        .map(|path| absolutize(base, path))
        .unwrap_or_else(|| base.to_path_buf());
    let candidates = global_values("scalar", "repo")?;
    if let Ok(enlistment) = resolve_enlistment(base, Some(&requested)) {
        let worktree = path_text(&enlistment.worktree);
        if candidates.iter().any(|candidate| candidate == &worktree) {
            return Ok(Some(worktree));
        }
    }
    let requested = canonicalize_existing_prefix(&requested);
    Ok(candidates.into_iter().find(|candidate| {
        let registered = canonicalize_existing_prefix(Path::new(candidate));
        registered == requested || registered == requested.join("src")
    }))
}

fn configure_repository(worktree: &Path, config_overrides: &[String]) -> Result<()> {
    for (key, value) in RECOMMENDED_CONFIG {
        if config_overrides.iter().any(|override_value| {
            override_value
                .split_once('=')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case(key))
        }) {
            continue;
        }
        crate::run(vec![
            "-C".into(),
            path_text(worktree),
            "config".into(),
            "set".into(),
            "--comment=set by scalar".into(),
            (*key).into(),
            (*value).into(),
        ])?;
    }
    Ok(())
}

fn start_maintenance(worktree: &Path) {
    trace_maintenance(&["start"]);
    if crate::run(vec![
        "-C".into(),
        path_text(worktree),
        "maintenance".into(),
        "start".into(),
    ])
    .is_err()
    {
        eprintln!("warning: could not toggle maintenance");
    }
}

fn stop_maintenance(worktree: &Path) -> Result<()> {
    trace_maintenance(&["unregister", "--force"]);
    crate::run(vec![
        "-C".into(),
        path_text(worktree),
        "maintenance".into(),
        "unregister".into(),
        "--force".into(),
    ])
}

fn trace_maintenance(args: &[&str]) {
    let mut argv = vec!["git".to_string(), "maintenance".to_string()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    sley::plumbing::sley_core::trace2::child_start("scalar", &argv);
}

#[cfg(unix)]
fn start_fsmonitor(worktree: &Path) -> Result<()> {
    let repository = Repository::discover(worktree)?;
    let daemon = FsmonitorDaemonSession::new(repository.git_dir());
    if daemon.state()? == FsmonitorDaemonState::Listening {
        return Ok(());
    }
    crate::run(vec![
        "-C".into(),
        path_text(worktree),
        "fsmonitor--daemon".into(),
        "start".into(),
    ])
    .map_err(|_| {
        eprintln!("error: could not start the FSMonitor daemon");
        GitError::Exit(1)
    })
}

#[cfg(not(unix))]
fn start_fsmonitor(_worktree: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn stop_fsmonitor(worktree: &Path) -> Result<()> {
    if !is_worktree_repository(worktree) {
        return Ok(());
    }
    let repository = Repository::discover(worktree)?;
    let daemon = FsmonitorDaemonSession::new(repository.git_dir());
    if daemon.state()? != FsmonitorDaemonState::Listening {
        return Ok(());
    }
    crate::run(vec![
        "-C".into(),
        path_text(worktree),
        "fsmonitor--daemon".into(),
        "stop".into(),
    ])
}

#[cfg(not(unix))]
fn stop_fsmonitor(_worktree: &Path) -> Result<()> {
    Ok(())
}

fn global_values(section: &str, key: &str) -> Result<Vec<String>> {
    let path = global_config_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(GitConfig::read(path)?
        .get_all(section, None, key)
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect())
}

fn add_global_value(section: &str, key: &str, value: &str) -> Result<()> {
    edit_global_config(|config| {
        if config
            .get_all(section, None, key)
            .into_iter()
            .any(|candidate| candidate == Some(value))
        {
            return;
        }
        let index = config
            .sections
            .iter()
            .rposition(|candidate| {
                candidate.name.eq_ignore_ascii_case(section) && candidate.subsection.is_none()
            })
            .unwrap_or_else(|| {
                config
                    .sections
                    .push(ConfigSection::new(section, None, Vec::new()));
                config.sections.len() - 1
            });
        config.sections[index]
            .entries
            .push(ConfigEntry::new(key, Some(value.to_string())));
    })
}

fn remove_global_value(section: &str, key: &str, value: &str) -> Result<()> {
    edit_global_config(|config| {
        for candidate in &mut config.sections {
            if !candidate.name.eq_ignore_ascii_case(section) || candidate.subsection.is_some() {
                continue;
            }
            candidate.entries.retain(|entry| {
                !(entry.key.eq_ignore_ascii_case(key) && entry.value.as_deref() == Some(value))
            });
        }
    })
}

fn edit_global_config(edit: impl FnOnce(&mut GitConfig)) -> Result<()> {
    let path = global_config_path()?;
    sley::plumbing::sley_config::raw_edit::edit_config_file_locked(
        path,
        sley::plumbing::sley_config::raw_edit::ConfigFileWriteOptions::default(),
        |original| {
            let mut config = if original.is_empty() {
                GitConfig::default()
            } else {
                GitConfig::parse(original)?
            };
            edit(&mut config);
            Ok::<_, GitError>(config.to_preserved_bytes())
        },
    )
    .map_err(|error| match error {
        sley::plumbing::sley_config::raw_edit::ConfigFileEditError::Edit(error) => error,
        sley::plumbing::sley_config::raw_edit::ConfigFileEditError::Write(error) => {
            GitError::Io(error.to_string())
        }
    })
}

fn global_config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("GIT_CONFIG_GLOBAL").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            eprintln!("fatal: $HOME not set");
            GitError::Exit(128)
        })?;
    Ok(PathBuf::from(home).join(".gitconfig"))
}

fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn discovery_blocked_by_ceiling(start: &Path, worktree: &Path) -> bool {
    if paths_equal(start, worktree) {
        return false;
    }
    let Some(value) = env::var_os("GIT_CEILING_DIRECTORIES") else {
        return false;
    };
    let ceilings = split_ceiling_directories(&value);
    for ancestor in start.ancestors().skip(1) {
        // Git applies a ceiling before probing that non-start ancestor for a
        // repository. A worktree which is itself the ceiling is thus hidden.
        if ceilings
            .iter()
            .any(|ceiling| paths_equal(ancestor, ceiling))
        {
            return true;
        }
        if paths_equal(ancestor, worktree) {
            return false;
        }
    }
    false
}

fn split_ceiling_directories(value: &std::ffi::OsStr) -> Vec<PathBuf> {
    env::split_paths(value)
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    canonicalize_existing_prefix(left) == canonicalize_existing_prefix(right)
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[derive(Debug, Default)]
struct CloneArgs {
    branch: Option<String>,
    full_clone: bool,
    single_branch: bool,
    src: bool,
    tags: bool,
    maintenance: bool,
    positional: Vec<String>,
}

fn clone(args: &[String], base: &Path) -> Result<()> {
    let parsed = parse_clone_args(args)?;
    let url = parsed
        .positional
        .first()
        .expect("clone parser requires a URL");
    let enlistment = parsed
        .positional
        .get(1)
        .cloned()
        .unwrap_or_else(|| clone_destination_from_url(url));
    let enlistment_root = absolutize(base, Path::new(&enlistment));
    if enlistment_root.is_dir() {
        eprintln!("fatal: directory '{enlistment}' exists already");
        return Err(GitError::Exit(128));
    }
    let destination = if parsed.src {
        enlistment_root.join("src")
    } else {
        enlistment_root
    };

    let show_progress = std::io::stderr().is_terminal();
    trace_scalar_fetch(show_progress, parsed.tags);

    let mut clone_args = vec![
        "-C".to_string(),
        path_text(base),
        "clone".to_string(),
        "--quiet".to_string(),
        if show_progress {
            "--progress".to_string()
        } else {
            "--no-progress".to_string()
        },
        "--filter=blob:none".to_string(),
    ];
    if !parsed.full_clone {
        clone_args.push("--sparse".into());
    }
    if parsed.single_branch {
        clone_args.push("--single-branch".into());
    }
    if let Some(branch) = &parsed.branch {
        clone_args.push("--branch".into());
        clone_args.push(branch.clone());
    }
    if !parsed.tags {
        clone_args.push("--no-tags".into());
    }
    for (key, value) in RECOMMENDED_CONFIG {
        clone_args.push("-c".into());
        clone_args.push(format!("{key}={value}"));
    }
    clone_args.push(url.clone());
    clone_args.push(destination.to_string_lossy().into_owned());
    crate::run(clone_args)?;

    let worktree = std::fs::canonicalize(&destination)?;
    crate::run(vec![
        "config".into(),
        "--global".into(),
        "--add".into(),
        "--no-fixed-value".into(),
        "scalar.repo".into(),
        worktree.to_string_lossy().into_owned(),
    ])?;
    if parsed.maintenance {
        sley::plumbing::sley_core::trace2::child_start(
            "scalar",
            &["git".into(), "maintenance".into(), "start".into()],
        );
        if crate::run(vec![
            "-C".into(),
            worktree.to_string_lossy().into_owned(),
            "maintenance".into(),
            "start".into(),
        ])
        .is_err()
        {
            eprintln!("warning: could not toggle maintenance");
        }
    }
    start_fsmonitor(&worktree)?;
    Ok(())
}

fn parse_clone_args(args: &[String]) -> Result<CloneArgs> {
    let mut parsed = CloneArgs {
        src: true,
        tags: true,
        maintenance: true,
        ..CloneArgs::default()
    };
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-b" | "--branch" => {
                index += 1;
                parsed.branch = Some(
                    args.get(index)
                        .ok_or_else(|| clone_usage_error("option `branch' requires a value"))?
                        .clone(),
                );
            }
            value if value.starts_with("--branch=") => {
                parsed.branch = Some(value["--branch=".len()..].to_string());
            }
            "--full-clone" => parsed.full_clone = true,
            "--no-full-clone" => parsed.full_clone = false,
            "--single-branch" => parsed.single_branch = true,
            "--no-single-branch" => parsed.single_branch = false,
            "--src" => parsed.src = true,
            "--no-src" => parsed.src = false,
            "--tags" => parsed.tags = true,
            "--no-tags" => parsed.tags = false,
            "--maintenance" => parsed.maintenance = true,
            "--no-maintenance" => parsed.maintenance = false,
            "-h" | "--help" => return clone_usage(),
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return clone_usage();
            }
            value => parsed.positional.push(value.to_string()),
        }
        index += 1;
    }
    if !(1..=2).contains(&parsed.positional.len()) {
        eprintln!("You must specify a repository to clone.");
        return clone_usage();
    }
    Ok(parsed)
}

fn clone_destination_from_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    trimmed
        .rsplit(['/', ':'])
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("repository")
        .to_string()
}

fn trace_scalar_fetch(progress: bool, tags: bool) {
    let mut argv = vec![
        "git".to_string(),
        "fetch".to_string(),
        "--quiet".to_string(),
        if progress {
            "--progress".to_string()
        } else {
            "--no-progress".to_string()
        },
        "origin".to_string(),
    ];
    if !tags {
        argv.push("--no-tags".into());
    }
    sley::plumbing::sley_core::trace2::child_start("scalar", &argv);
}

fn clone_usage_error(message: &str) -> GitError {
    eprintln!("error: {message}");
    GitError::Exit(129)
}

fn clone_usage<T>() -> Result<T> {
    eprintln!("{CLONE_USAGE}");
    Err(GitError::Exit(129))
}

fn diagnose(args: &[String], base: &Path) -> Result<()> {
    if args.len() > 1 || args.first().is_some_and(|arg| arg.starts_with('-')) {
        return usage();
    }

    let requested = args
        .first()
        .map(|arg| absolutize(base, Path::new(arg)))
        .unwrap_or_else(|| base.to_path_buf());
    if !requested.is_dir() {
        eprintln!("fatal: '{}' does not exist", requested.display());
        return Err(GitError::Exit(128));
    }

    let src = requested.join("src");
    let (repository, diagnostics_root) = if is_worktree_repository(&src) {
        (Repository::discover(&src)?, requested)
    } else {
        let repository = Repository::discover(&requested).map_err(|_| {
            eprintln!("fatal: not a git repository (or any of the parent directories): .git");
            GitError::Exit(128)
        })?;
        let diagnostics_root = repository.workdir().ok_or_else(scalar_requires_worktree)?;
        (repository, diagnostics_root)
    };
    let worktree = repository.workdir().ok_or_else(scalar_requires_worktree)?;
    let output = diagnostics_root.join(".scalarDiagnostics");

    crate::run(vec![
        "-C".into(),
        worktree.to_string_lossy().into_owned(),
        "diagnose".into(),
        "--mode=all".into(),
        "-s".into(),
        "%Y%m%d_%H%M%S".into(),
        "-o".into(),
        output.to_string_lossy().into_owned(),
    ])
}

fn is_worktree_repository(path: &Path) -> bool {
    let dot_git = path.join(".git");
    dot_git.is_dir() || dot_git.is_file()
}

fn absolutize(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn scalar_requires_worktree() -> GitError {
    eprintln!("fatal: Scalar enlistments require a worktree");
    GitError::Exit(128)
}

fn usage<T>() -> Result<T> {
    eprintln!("{USAGE}");
    Err(GitError::Exit(129))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "sley-scalar-unit-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create Scalar unit fixture");
        root
    }

    #[test]
    fn repeated_global_c_parsing_carries_base_without_changing_process_cwd() {
        let process_cwd = env::current_dir().expect("process cwd");
        let root = fixture_root("global-c");
        let first = root.join("first");
        let second = first.join("second");
        fs::create_dir_all(&second).expect("create nested invocation bases");

        let first_args = [
            "-C".into(),
            root.to_string_lossy().into_owned(),
            "-C".into(),
            "first".into(),
            "-C".into(),
            "second".into(),
            "list".into(),
        ];
        let parsed = parse_global_args_from(&first_args, process_cwd.clone())
            .expect("parse chained -C options");
        assert_eq!(parsed.base, second.canonicalize().expect("canonical base"));
        assert_eq!(parsed.command, Some("list"));
        assert_eq!(
            env::current_dir().expect("cwd after first parse"),
            process_cwd
        );

        let repeated_args = ["list".into()];
        let repeated = parse_global_args_from(&repeated_args, process_cwd.clone())
            .expect("parse a second invocation");
        assert_eq!(repeated.base, process_cwd);
        assert_eq!(env::current_dir().expect("cwd after repeat"), repeated.base);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ceiling_directory_list_uses_platform_path_separator() {
        let paths = [PathBuf::from("one"), PathBuf::from("two")];
        let encoded = env::join_paths(&paths).expect("encode platform path list");
        assert_eq!(split_ceiling_directories(&encoded), paths);
    }
}
