//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;
use sley::plumbing::{sley_core, sley_index, sley_worktree};

pub(crate) fn cmd_clean(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut dry_run = false;
    let mut force_count = 0u8;
    let mut force_was_mentioned = false;
    let mut directories = false;
    let mut ignore_mode = CleanIgnoreMode::Normal;
    let mut interactive = false;
    let mut quiet = false;
    let mut excludes = Vec::new();
    let mut path_args = Vec::new();
    let mut parsing_options = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            path_args.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-f" | "--force" => {
                force_count = force_count.saturating_add(1);
                force_was_mentioned = true;
            }
            "-ff" => {
                force_count = force_count.saturating_add(2);
                force_was_mentioned = true;
            }
            "--no-force" => {
                force_count = 0;
                force_was_mentioned = true;
            }
            "-d" => directories = true,
            "-x" => ignore_mode = CleanIgnoreMode::Include,
            "-X" => ignore_mode = CleanIgnoreMode::Only,
            "-i" | "--interactive" => interactive = true,
            "-e" | "--exclude" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("clean --exclude requires a value".into()))?;
                excludes.push(value.to_string());
            }
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--no-interactive" => {}
            value
                if value.starts_with('-')
                    && !value.starts_with("--")
                    && value.len() > 2
                    && value[1..].bytes().all(|byte| {
                        matches!(byte, b'f' | b'd' | b'n' | b'q' | b'x' | b'X' | b'i')
                    }) =>
            {
                for byte in value[1..].bytes() {
                    match byte {
                        b'n' => dry_run = true,
                        b'f' => {
                            force_count = force_count.saturating_add(1);
                            force_was_mentioned = true;
                        }
                        b'd' => directories = true,
                        b'q' => quiet = true,
                        b'x' => ignore_mode = CleanIgnoreMode::Include,
                        b'X' => ignore_mode = CleanIgnoreMode::Only,
                        b'i' => interactive = true,
                        _ => {}
                    }
                }
            }
            "--" => parsing_options = false,
            value if value.starts_with("--exclude=") => {
                let value = value
                    .strip_prefix("--exclude=")
                    .ok_or_else(|| GitError::Command("clean --exclude requires a value".into()))?;
                excludes.push(value.to_string());
            }
            value => path_args.push(value.to_string()),
        }
    }
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let config = read_repo_config(&git_dir)?;
    let require_force = config
        .get_bool("clean", None, "requireForce")
        .unwrap_or(true);
    if interactive {
        print_clean_interactive_stub()?;
        return Ok(());
    }
    if !dry_run && force_count == 0 && require_force {
        if force_was_mentioned {
            eprintln!("fatal: clean.requireForce is true and -f not given: refusing to clean");
        } else {
            eprintln!(
                "fatal: clean.requireForce defaults to true and neither -i, -n, nor -f given; refusing to clean"
            );
        }
        return Err(GitError::Exit(128));
    }
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let pathspec = LsFilesPathspec::new(&cwd, &worktree_root, false, &path_args)?;
    let paths = clean_targets(
        &worktree_root,
        &git_dir,
        format,
        directories,
        ignore_mode,
        &pathspec,
        &excludes,
    )?;
    clean_trace2_directories_visited(1);
    let mut stdout = io::stdout();
    for target in paths {
        if force_count < 2 && clean_target_is_nested_repository(&worktree_root, &target)? {
            continue;
        }
        let display = String::from_utf8_lossy(&target.display);
        if dry_run {
            writeln!(stdout, "Would remove {display}")?;
            continue;
        }
        if !quiet {
            writeln!(stdout, "Removing {display}")?;
        }
        let mut filesystem_path = target.path;
        if filesystem_path.ends_with(b"/") {
            filesystem_path.pop();
        }
        let relative = std::str::from_utf8(&filesystem_path)
            .map_err(|err| GitError::InvalidPath(err.to_string()))?;
        let absolute = worktree_root.join(relative);
        if target.is_dir {
            if clean_target_is_original_cwd(&absolute) {
                clean_original_cwd_contents(&absolute, &excludes)?;
                write!(stdout, "Refusing to remove current working directory\n")?;
                continue;
            }
            match fs::remove_dir_all(&absolute) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                    fs::remove_dir(&absolute)?;
                }
                Err(err) => return Err(err.into()),
            }
        } else {
            fs::remove_file(absolute)?;
        }
    }
    Ok(())
}

fn clean_target_is_original_cwd(path: &Path) -> bool {
    let Some(cwd) = sley_core::original_cwd().or_else(|| env::current_dir().ok()) else {
        return false;
    };
    let cwd = fs::canonicalize(&cwd).unwrap_or(cwd);
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path == cwd
}

fn clean_original_cwd_contents(path: &Path, excludes: &[String]) -> Result<()> {
    let read = match fs::read_dir(path) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if excludes.iter().any(|exclude| exclude == &name) {
            continue;
        }
        let child = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            fs::remove_dir_all(child)?;
        } else {
            fs::remove_file(child)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CleanIgnoreMode {
    Normal,
    Include,
    Only,
}

fn print_clean_interactive_stub() -> Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "*** Commands ***")?;
    writeln!(
        stdout,
        "    1: clean                2: filter by pattern    3: select by numbers"
    )?;
    writeln!(
        stdout,
        "    4: ask each             5: quit                 6: help"
    )?;
    stdout.flush()?;
    Ok(())
}

fn clean_trace2_directories_visited(value: usize) {
    let Some(target) = std::env::var_os("GIT_TRACE2_PERF") else {
        return;
    };
    let target = target.to_string_lossy().into_owned();
    if !target.starts_with('/') {
        return;
    }
    let line = format!(
        "19:00:00.000000 file.c:1 | d0 | main | data | r1 | ? | ? | read_directory | ..directories-visited:{value}\n"
    );
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)
    {
        let _ = file.write_all(line.as_bytes());
    }
}
struct CleanTarget {
    path: Vec<u8>,
    display: Vec<u8>,
    is_dir: bool,
}

fn clean_targets(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    directories: bool,
    ignore_mode: CleanIgnoreMode,
    pathspec: &LsFilesPathspec,
    excludes: &[String],
) -> Result<Vec<CleanTarget>> {
    let has_pathspec = !pathspec.filters.is_empty();
    // Git treats any pathspec as `-d` for selection purposes.
    let effective_directories = directories || has_pathspec;
    let index = sley_worktree::read_repository_index(git_dir, format)?;
    let exclude_patterns = clean_exclude_patterns(git_dir, format, index.as_ref(), excludes)?;

    let mut paths = if effective_directories {
        sley_worktree::untracked_paths_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory: true,
                no_empty_directory: false,
                preserve_ignored_directories: directories && ignore_mode == CleanIgnoreMode::Normal,
                exclude_standard: ignore_mode != CleanIgnoreMode::Include,
                ignored_only: ignore_mode == CleanIgnoreMode::Only,
                exclude_patterns: exclude_patterns.clone(),
                exclude_per_directory: Vec::new(),
                pathspecs: pathspec.untracked_pathspecs(),
            },
        )?
    } else {
        sley_worktree::untracked_paths_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory: false,
                no_empty_directory: false,
                preserve_ignored_directories: false,
                exclude_standard: ignore_mode != CleanIgnoreMode::Include,
                ignored_only: ignore_mode == CleanIgnoreMode::Only,
                exclude_patterns,
                exclude_per_directory: Vec::new(),
                pathspecs: pathspec.untracked_pathspecs(),
            },
        )?
    };

    // Without `-d` (and without a pathspec, which Git treats as `-d`), the
    // non-directory walk lists every untracked file. Git only removes a file in
    // a subdirectory when that directory contains tracked content; an untracked
    // file inside a wholly-untracked directory needs `-d`. The directory walk
    // already encodes this selection (it rolls wholly-untracked directories up
    // to `dir/` and only descends into directories with tracked/ignored content),
    // so the retain must run only on the non-directory walk's flat output.
    if !effective_directories {
        paths.retain(|path| {
            path.ends_with(b"/") || clean_untracked_file_eligible(path, index.as_ref())
        });
    }

    if has_pathspec {
        paths = clean_collapse_untracked_paths(paths);
    }

    let mut targets = Vec::new();
    for path in paths {
        let is_dir = path.ends_with(b"/");
        let Some(display) = pathspec.display(&path) else {
            continue;
        };
        targets.push(CleanTarget {
            path,
            display,
            is_dir,
        });
    }

    targets.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(targets)
}

fn clean_exclude_patterns(
    git_dir: &Path,
    format: ObjectFormat,
    index: Option<&Index>,
    excludes: &[String],
) -> Result<Vec<Vec<u8>>> {
    let mut patterns = excludes
        .iter()
        .map(|exclude| exclude.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let Some(index) = index else {
        return Ok(patterns);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    for entry in &index.entries {
        if entry.stage() != sley_index::Stage::Normal
            || !entry.is_skip_worktree()
            || entry.path.as_bytes() != b".gitignore"
        {
            continue;
        }
        let object = db.read_object(&entry.oid)?;
        if object.object_type != ObjectType::Blob {
            continue;
        }
        for line in object.body.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            patterns.push(line.to_vec());
        }
    }
    Ok(patterns)
}

fn clean_target_is_nested_repository(worktree_root: &Path, target: &CleanTarget) -> Result<bool> {
    if !target.is_dir {
        return Ok(false);
    }
    let path = target.path.strip_suffix(b"/").unwrap_or(&target.path);
    let relative =
        std::str::from_utf8(path).map_err(|err| GitError::InvalidPath(err.to_string()))?;
    let absolute = worktree_root.join(relative);
    clean_directory_contains_nested_repository(&absolute)
}

fn clean_directory_contains_nested_repository(path: &Path) -> Result<bool> {
    if clean_directory_is_nested_repository(path)? {
        return Ok(true);
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name() == std::ffi::OsStr::new(".git") {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() && clean_directory_contains_nested_repository(&entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn clean_directory_is_nested_repository(absolute: &Path) -> Result<bool> {
    let dot_git = absolute.join(".git");
    if dot_git.is_dir() {
        return clean_git_dir_path_is_repository(&dot_git);
    }
    if dot_git.is_file() {
        match read_gitdir_file(&dot_git) {
            Ok(Some(git_dir)) => return Ok(git_dir.exists()),
            Ok(None) => return Ok(false),
            Err(GitError::Io(_)) => return Ok(true),
            Err(err) => return Err(err),
        }
    }
    Ok(false)
}

fn clean_git_dir_path_is_repository(git_dir: &Path) -> Result<bool> {
    if !is_git_dir_candidate(git_dir) {
        return Ok(false);
    }
    let head = match fs::read_to_string(git_dir.join("HEAD")) {
        Ok(head) => head,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    let head = head.trim();
    Ok(head.starts_with("ref: ") || clean_head_is_hex_object_name(head))
}

fn clean_head_is_hex_object_name(head: &str) -> bool {
    matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// regardless of `-x` or whether the repository has any commits yet.
fn clean_untracked_file_eligible(path: &[u8], index: Option<&Index>) -> bool {
    if !path.iter().any(|byte| *byte == b'/') {
        return true;
    }
    let Some(index) = index else {
        return false;
    };
    clean_path_parent(path).is_some_and(|parent| clean_index_has_tracked_under(index, parent))
}

fn clean_index_has_tracked_under(index: &Index, directory: &[u8]) -> bool {
    let mut prefix = directory.to_vec();
    prefix.push(b'/');
    index
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes().starts_with(&prefix))
}

fn clean_path_parent(path: &[u8]) -> Option<&[u8]> {
    let slash = path.iter().rposition(|byte| *byte == b'/')?;
    if slash == 0 {
        return None;
    }
    Some(&path[..slash])
}

/// Match git `correct_untracked_entries` for pathspec-driven clean.
fn clean_collapse_untracked_paths(paths: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    // The directory walk already encodes Git's `--directory` rollup: a
    // wholly-untracked directory named by a pathspec is emitted as `dir/`, while
    // untracked files inside a partially-tracked directory are listed
    // individually. The only post-processing left is dropping a file entry that
    // is already subsumed by a rolled-up parent directory entry.
    let mut sorted = paths;
    sorted.sort();
    let mut kept = BTreeSet::new();
    for path in &sorted {
        if sorted.iter().any(|other| {
            other != path && other.ends_with(b"/") && clean_directory_contains_path(other, path)
        }) {
            continue;
        }
        kept.insert(path.clone());
    }
    kept.into_iter().collect()
}

fn clean_directory_contains_path(directory: &[u8], path: &[u8]) -> bool {
    directory.strip_suffix(b"/").is_some_and(|directory| {
        path.strip_prefix(directory)
            .and_then(|rest| rest.strip_prefix(b"/"))
            .is_some()
    })
}
