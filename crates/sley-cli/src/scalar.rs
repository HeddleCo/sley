//! Native implementation of Scalar's auxiliary command surface.
//!
//! Scalar remains a separate executable, but repository work is dispatched
//! in-process through Sley. This module owns only Scalar-specific enlistment
//! discovery and argument translation.

use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use sley::{GitError, Repository, Result};

const USAGE: &str = "usage: scalar diagnose [<enlistment>]";
const CLONE_USAGE: &str = "usage: scalar clone [--single-branch] [--branch <main-branch>] [--full-clone]\n\t[--[no-]src] [--[no-]tags] [--[no-]maintenance] <url> [<enlistment>]";

const RECOMMENDED_CONFIG: &[(&str, &str)] = &[
    ("am.keepCR", "true"),
    ("commitGraph.changedPaths", "true"),
    ("commitGraph.generationVersion", "1"),
    ("core.autoCRLF", "false"),
    ("core.safeCRLF", "false"),
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
    let Some((command, rest)) = args.split_first() else {
        return usage();
    };
    match command.as_str() {
        "clone" => clone(rest),
        "diagnose" => diagnose(rest),
        "-h" | "--help" => usage(),
        _ => usage(),
    }
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

fn clone(args: &[String]) -> Result<()> {
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
    if Path::new(&enlistment).is_dir() {
        eprintln!("fatal: directory '{enlistment}' exists already");
        return Err(GitError::Exit(128));
    }
    let destination = if parsed.src {
        PathBuf::from(&enlistment).join("src")
    } else {
        PathBuf::from(&enlistment)
    };

    let show_progress = std::io::stderr().is_terminal();
    trace_scalar_fetch(show_progress, parsed.tags);

    let mut clone_args = vec![
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

fn diagnose(args: &[String]) -> Result<()> {
    if args.len() > 1 || args.first().is_some_and(|arg| arg.starts_with('-')) {
        return usage();
    }

    let invocation_dir = env::current_dir()?;
    let requested = args
        .first()
        .map(|arg| absolutize(&invocation_dir, Path::new(arg)))
        .unwrap_or_else(|| invocation_dir.clone());
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
        let diagnostics_root = repository
            .workdir()
            .ok_or_else(|| scalar_requires_worktree())?;
        (repository, diagnostics_root)
    };
    let worktree = repository
        .workdir()
        .ok_or_else(|| scalar_requires_worktree())?;
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
