//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;
use sley::plumbing::{sley_index, sley_worktree};

use super::add::{
    active_sparse_checkout_for_add, add_git_path_bytes, add_index_entries_path_range,
    advise_on_updating_sparse_paths, normalize_add_absolute_path,
};

pub(crate) fn cmd_rm(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut paths = Vec::new();
    let mut recursive = false;
    let mut quiet = false;
    let mut cached = false;
    let mut force = false;
    let mut dry_run = false;
    let mut ignore_unmatch = false;
    let mut sparse = false;
    let mut parsing_options = true;
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
            "-r" | "-R" | "--recursive" => recursive = true,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--cached" => cached = true,
            "--no-cached" => cached = false,
            "--ignore-unmatch" => ignore_unmatch = true,
            "--no-ignore-unmatch" => ignore_unmatch = false,
            "--sparse" => sparse = true,
            "--no-sparse" => sparse = false,
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
                    GitError::Command("--pathspec-from-file requires a value".into())
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
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|option| matches!(option, b'r' | b'R' | b'f' | b'n' | b'q')) =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b'r' | b'R' => recursive = true,
                        b'f' => force = true,
                        b'n' => dry_run = true,
                        b'q' => quiet = true,
                        _ => unreachable!("rm short-option group was filtered"),
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported rm option {value}")));
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
        eprintln!("fatal: No pathspec was given. Which files should I remove?");
        return Err(GitError::Exit(128));
    }
    if paths.iter().any(|path| path.as_os_str().is_empty()) {
        eprintln!(
            "fatal: empty string is not a valid pathspec. please use . instead if you meant to match all paths"
        );
        return Err(GitError::Exit(128));
    }
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let path_base = match cwd.strip_prefix(&worktree_root) {
        Ok(_) => cwd.as_path(),
        Err(_) => worktree_root.as_path(),
    };
    let resolved_paths = paths
        .into_iter()
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                path_base.join(path)
            }
        })
        .collect::<Vec<_>>();
    let config_parameters_env = effective_config_parameters_env();
    let result = sley_worktree::remove_index_and_worktree_paths(
        worktree_root,
        git_dir,
        format,
        &resolved_paths,
        sley_worktree::RemoveOptions {
            recursive,
            cached,
            force,
            dry_run,
            ignore_unmatch,
            sparse,
        },
        config_parameters_env.as_deref(),
    )?;
    if !quiet {
        let mut stdout = io::stdout().lock();
        for path in result.removed {
            if let Err(err) = writeln!(stdout, "rm '{}'", String::from_utf8_lossy(&path)) {
                if err.kind() == io::ErrorKind::BrokenPipe {
                    std::process::exit(141);
                }
                return Err(err.into());
            }
        }
    }
    Ok(())
}

pub(crate) fn cmd_mv(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut paths = Vec::new();
    let mut force = false;
    let mut dry_run = false;
    let mut verbose = false;
    let mut skip_errors = false;
    let mut ignore_sparse = false;
    let mut parsing_options = true;
    for arg in args {
        if !parsing_options {
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-k" => skip_errors = true,
            "--sparse" => ignore_sparse = true,
            "--no-sparse" => ignore_sparse = false,
            value if value.starts_with('-') && !value.starts_with("--") && value.len() > 2 => {
                for flag in value[1..].bytes() {
                    match flag {
                        b'f' => force = true,
                        b'n' => dry_run = true,
                        b'v' => verbose = true,
                        b'k' => skip_errors = true,
                        other => {
                            return Err(GitError::Command(format!(
                                "unsupported mv option -{}",
                                other as char
                            )));
                        }
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported mv option {value}")));
            }
            value => paths.push(PathBuf::from(value)),
        }
    }
    if paths.len() < 2 {
        return Err(GitError::Command(
            "mv currently supports <source>... <destination>".into(),
        ));
    }
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let destination = if paths[paths.len() - 1].is_absolute() {
        paths[paths.len() - 1].clone()
    } else {
        cwd.join(&paths[paths.len() - 1])
    };
    if paths.len() > 2 && !destination.is_dir() {
        eprintln!(
            "fatal: destination '{}' is not a directory",
            destination.display()
        );
        return Err(GitError::Exit(128));
    }
    if paths.len() > 2 {
        validate_mv_sources_do_not_overlap(&cwd, &worktree_root, &paths[..paths.len() - 1])?;
    }

    // git refuses to move a source or destination that the sparse-checkout
    // definition places outside the working set (builtin/mv.c's
    // only_match_skip_worktree handling); `--sparse` opts out. Compute which
    // source/destination pairs are sparse so they can be skipped, and emit the
    // shared advice. Without `-k` any sparse match aborts the whole command.
    let sources = &paths[..paths.len() - 1];
    let source_skipped = if ignore_sparse {
        vec![false; sources.len()]
    } else {
        let (rejected_paths, per_source) = mv_sparse_rejections(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            sources,
            &destination,
        )?;
        if !rejected_paths.is_empty() {
            advise_on_updating_sparse_paths(&git_dir, &rejected_paths);
            if !skip_errors {
                return Err(GitError::Exit(1));
            }
        }
        per_source
    };

    let mut results = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        if source_skipped[index] {
            continue;
        }
        let source = if source.is_absolute() {
            source.clone()
        } else {
            cwd.join(source)
        };
        let result = sley_worktree::move_index_and_worktree_path(
            &worktree_root,
            &git_dir,
            format,
            &source,
            &destination,
            sley_worktree::MoveOptions {
                force,
                dry_run,
                skip_errors,
                sparse: ignore_sparse,
            },
        )?;
        let fatal = result.fatal.is_some();
        results.push(result);
        if dry_run && fatal {
            break;
        }
    }
    if dry_run {
        for result in &results {
            let source = String::from_utf8_lossy(&result.source);
            let destination = String::from_utf8_lossy(&result.destination);
            println!("Checking rename of '{source}' to '{destination}'");
            for detail in &result.details {
                let source = String::from_utf8_lossy(&detail.source);
                let destination = String::from_utf8_lossy(&detail.destination);
                println!("Checking rename of '{source}' to '{destination}'");
            }
        }
        if let Some(fatal) = results.iter().find_map(|result| result.fatal.as_deref()) {
            eprintln!("{fatal}");
            return Err(GitError::Exit(128));
        }
    }
    if dry_run || verbose {
        for result in &results {
            if result.skipped {
                continue;
            }
            let source = String::from_utf8_lossy(&result.source);
            let destination = String::from_utf8_lossy(&result.destination);
            println!("Renaming {source} to {destination}");
            for detail in &result.details {
                if detail.skipped {
                    continue;
                }
                let source = String::from_utf8_lossy(&detail.source);
                let destination = String::from_utf8_lossy(&detail.destination);
                println!("Renaming {source} to {destination}");
            }
        }
    }
    Ok(())
}

/// For each `git mv` (source, destination) pair, decide whether the source
/// and/or destination fall outside the sparse-checkout definition, mirroring
/// builtin/mv.c's `only_match_skip_worktree` collection. Returns the offending
/// git paths (source before destination, matching git's append order) and a
/// per-source flag marking which moves must be skipped.
///
/// A source is sparse when it lies outside the sparse cone, or when it is an
/// absent skip-worktree index entry (git's "lstat fails + ce_skip_worktree"
/// branch). A destination is sparse when it lies outside the cone.
fn mv_sparse_rejections(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    sources: &[PathBuf],
    destination: &Path,
) -> Result<(Vec<String>, Vec<bool>)> {
    let Some(active) = active_sparse_checkout_for_add(git_dir)? else {
        return Ok((Vec::new(), vec![false; sources.len()]));
    };
    let index = sley_worktree::read_repository_index(git_dir, format)?;
    // git treats a destination directory that the sparse-checkout removed from
    // disk (but still tracks) as a directory; detect that from the index so a
    // contained file's mapped destination path is computed correctly.
    let dest_is_dir = destination.is_dir()
        || mv_git_relative_path(worktree_root, destination).is_some_and(|dest_git| {
            let mut prefix = dest_git;
            prefix.push(b'/');
            index.as_ref().is_some_and(|index| {
                index
                    .entries
                    .iter()
                    .any(|entry| entry.path.as_bytes().starts_with(&prefix))
            })
        });
    let mut rejected = Vec::new();
    let mut per_source = vec![false; sources.len()];
    let in_cone = |git_path: &[u8]| {
        sley_worktree::path_in_sparse_checkout(git_path, &active.sparse, active.mode)
    };
    for (i, source) in sources.iter().enumerate() {
        let source_abs = normalize_add_absolute_path(cwd, source);
        let dest_abs = if dest_is_dir {
            match source_abs.file_name() {
                Some(name) => destination.join(name),
                None => destination.to_path_buf(),
            }
        } else {
            destination.to_path_buf()
        };
        let Some(src_git) = mv_git_relative_path(worktree_root, &source_abs) else {
            continue;
        };
        let dst_git = mv_git_relative_path(worktree_root, &dest_abs);
        // A directory source (still tracked under a prefix even after its files
        // were sparsified off disk) expands to its contained entries: git lists
        // each contained file's source and mapped destination that is sparse.
        let mut prefix = src_git.clone();
        prefix.push(b'/');
        let contained: Vec<&IndexEntry> = index
            .as_ref()
            .map(|index| {
                index
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.stage() == sley_index::Stage::Normal
                            && entry.path.as_bytes().starts_with(&prefix)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if source_abs.is_dir() || !contained.is_empty() {
            for entry in contained {
                let name = entry.path.as_bytes();
                if !in_cone(name) {
                    rejected.push(String::from_utf8_lossy(name).into_owned());
                    per_source[i] = true;
                }
                if let Some(dst_git) = dst_git.as_ref() {
                    let mut mapped = dst_git.clone();
                    mapped.extend_from_slice(&name[src_git.len()..]);
                    if !in_cone(&mapped) {
                        rejected.push(String::from_utf8_lossy(&mapped).into_owned());
                        per_source[i] = true;
                    }
                }
            }
            continue;
        }
        let present = fs::symlink_metadata(&source_abs).is_ok();
        if !in_cone(&src_git)
            || (!present && mv_index_entry_skip_worktree(index.as_ref(), &src_git))
        {
            rejected.push(String::from_utf8_lossy(&src_git).into_owned());
            per_source[i] = true;
        }
        if let Some(dst_git) = dst_git.as_ref()
            && !in_cone(dst_git)
        {
            rejected.push(String::from_utf8_lossy(dst_git).into_owned());
            per_source[i] = true;
        }
    }
    Ok((rejected, per_source))
}

fn mv_git_relative_path(worktree_root: &Path, absolute: &Path) -> Option<Vec<u8>> {
    let relative = absolute.strip_prefix(worktree_root).ok()?;
    let git_path = add_git_path_bytes(relative).ok()?;
    (!git_path.is_empty()).then_some(git_path)
}

fn mv_index_entry_skip_worktree(index: Option<&Index>, git_path: &[u8]) -> bool {
    index.is_some_and(|index| {
        let range = add_index_entries_path_range(&index.entries, git_path);
        index.entries[range]
            .iter()
            .any(|entry| entry.stage() == sley_index::Stage::Normal && entry.is_skip_worktree())
    })
}

fn validate_mv_sources_do_not_overlap(
    cwd: &Path,
    worktree_root: &Path,
    sources: &[PathBuf],
) -> Result<()> {
    let mut normalized = Vec::new();
    for source in sources {
        let absolute = if source.is_absolute() {
            source.clone()
        } else {
            cwd.join(source)
        };
        let absolute = normalize_mv_absolute_path_lexically(&absolute);
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", source.display()))
        })?;
        let path = mv_git_path_bytes(relative)?;
        normalized.push(path);
    }
    for (left_index, left) in normalized.iter().enumerate() {
        for right in normalized.iter().skip(left_index + 1) {
            if mv_path_is_parent(left, right) {
                print_mv_parent_child_error(right, left);
                return Err(GitError::Exit(128));
            }
            if mv_path_is_parent(right, left) {
                print_mv_parent_child_error(left, right);
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(())
}

fn mv_path_is_parent(parent: &[u8], child: &[u8]) -> bool {
    child.len() > parent.len()
        && child.starts_with(parent)
        && child.get(parent.len()) == Some(&b'/')
}

fn print_mv_parent_child_error(child: &[u8], parent: &[u8]) {
    eprintln!(
        "fatal: cannot move both '{}' and its parent directory '{}'",
        String::from_utf8_lossy(child),
        String::from_utf8_lossy(parent)
    );
}

fn mv_git_path_bytes(path: &Path) -> Result<Vec<u8>> {
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

fn normalize_absolute_path_lexically(path: &Path) -> PathBuf {
    normalize_mv_absolute_path_lexically(path)
}

fn normalize_mv_absolute_path_lexically(path: &Path) -> PathBuf {
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
