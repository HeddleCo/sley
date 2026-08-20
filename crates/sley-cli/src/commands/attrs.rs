//! Attribute and ignore inspection commands (`check-attr`, `check-ignore`).

use crate::sley_object;
use sley::plumbing::{sley_core, sley_index, sley_rev, sley_worktree};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sley::plumbing::sley_object::tree_entry_object_type;
use sley::{GitError, Result};
use sley_pathspec::normalize_ls_files_pathspec;

use crate::{
    RepositoryContext, check_ignore_tracked_paths, global_attr_source, require_work_tree,
    resolve_cli_path, status_quote_path, worktree_prefix, write_check_attr_state,
};

pub(crate) fn cmd_check_ignore(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut read_stdin = false;
    let mut quiet = false;
    let mut verbose = false;
    let mut non_matching = false;
    let mut z = false;
    let mut no_index = false;
    let mut path_args = Vec::new();
    let mut end_options = false;
    for arg in args {
        if end_options {
            path_args.push(arg.as_bytes().to_vec());
            continue;
        }
        match arg.as_str() {
            "--" => end_options = true,
            "--stdin" => read_stdin = true,
            "--no-stdin" => read_stdin = false,
            "--no-index" => no_index = true,
            "--index" => no_index = false,
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-n" | "--non-matching" => non_matching = true,
            "--no-non-matching" => non_matching = false,
            "-z" => z = true,
            value
                if value.starts_with('-')
                    && !value.starts_with("--")
                    && value.len() > 2
                    && value[1..]
                        .chars()
                        .all(|flag| matches!(flag, 'q' | 'v' | 'n' | 'z')) =>
            {
                for flag in value[1..].chars() {
                    match flag {
                        'q' => quiet = true,
                        'v' => verbose = true,
                        'n' => non_matching = true,
                        'z' => z = true,
                        _ => {}
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "check-ignore currently supports path arguments, --stdin, --no-index, -z, -q, -v, and -n; unsupported option {value}"
                )));
            }
            _ => path_args.push(arg.as_bytes().to_vec()),
        }
    }
    if read_stdin {
        if !path_args.is_empty() {
            eprintln!("fatal: cannot specify pathnames with --stdin");
            return Err(GitError::Exit(128));
        }
    } else {
        if z {
            eprintln!("fatal: -z only makes sense with --stdin");
            return Err(GitError::Exit(128));
        }
        if path_args.is_empty() {
            eprintln!("fatal: no path specified");
            return Err(GitError::Exit(128));
        }
    }
    if quiet {
        if path_args.len() > 1 {
            eprintln!("fatal: --quiet is only valid with a single pathname");
            return Err(GitError::Exit(128));
        }
        if verbose {
            eprintln!("fatal: cannot have both --quiet and --verbose");
            return Err(GitError::Exit(128));
        }
    }
    if non_matching && !verbose {
        eprintln!("fatal: --non-matching is only valid with --verbose");
        return Err(GitError::Exit(128));
    }

    let repo = RepositoryContext::from_session(cli_session)?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let worktree_root = &require_work_tree(cli_session, git_dir)?;
    let prefix = worktree_prefix(cli_session, cwd, git_dir)?;
    let (tracked_paths, gitlink_paths) = if no_index {
        (BTreeSet::new(), Vec::new())
    } else {
        check_ignore_tracked_paths(git_dir, format)?
    };
    let mut stdout = io::stdout().lock();
    let terminator = if z { b'\0' } else { b'\n' };
    let mut matched_any = false;
    let process_path = |display_path: Vec<u8>,
                        stdout: &mut std::io::StdoutLock<'_>|
     -> Result<bool> {
        let path_arg = String::from_utf8_lossy(&display_path);
        let git_path = normalize_ls_files_pathspec(prefix.as_bytes(), &path_arg)?;
        validate_check_ignore_pathspec(worktree_root, &git_path, &display_path, &gitlink_paths)?;
        let absolute = resolve_cli_path(cwd, &path_arg);
        // Tracked paths are never ignored (upstream check-ignore consults the
        // index via find_pathspecs_matching_against_index and skips matching
        // entries): report them as non-matching rather than skipping output,
        // so `-v -n` still prints the `::` line for them.
        let ignore_match = if tracked_paths.contains(&git_path) {
            None
        } else {
            sley_worktree::standard_ignore_match(worktree_root, &git_path, absolute.is_dir())?
        };
        let path_matched = ignore_match
            .as_ref()
            .is_some_and(|ignore_match| verbose || ignore_match.ignored);
        if let Some(ignore_match) = ignore_match {
            if !quiet && verbose {
                if z {
                    stdout.write_all(&ignore_match.source)?;
                    stdout.write_all(&[0])?;
                    write!(stdout, "{}", ignore_match.line_number)?;
                    stdout.write_all(&[0])?;
                    stdout.write_all(&ignore_match.pattern)?;
                    stdout.write_all(&[0])?;
                    stdout.write_all(&display_path)?;
                    stdout.write_all(&[0])?;
                } else {
                    stdout.write_all(&ignore_match.source)?;
                    write!(stdout, ":{}:", ignore_match.line_number)?;
                    stdout.write_all(&ignore_match.pattern)?;
                    stdout.write_all(b"\t")?;
                    write_check_ignore_quoted(stdout, &display_path)?;
                    stdout.write_all(&[terminator])?;
                }
            } else if !quiet && ignore_match.ignored {
                stdout.write_all(&display_path)?;
                stdout.write_all(&[terminator])?;
            }
        } else if verbose && non_matching && !quiet {
            if z {
                stdout.write_all(&[0, 0, 0])?;
                stdout.write_all(&display_path)?;
                stdout.write_all(&[0])?;
            } else {
                stdout.write_all(b"::\t")?;
                write_check_ignore_quoted(stdout, &display_path)?;
                stdout.write_all(&[terminator])?;
            }
        }
        Ok(path_matched)
    };
    if read_stdin {
        crate::commands::stdin_stream::stream_stdin_records(
            terminator,
            &mut stdout,
            |mut display_path, stdout| {
                if display_path.is_empty() {
                    return Ok(());
                }
                if !z {
                    crate::commands::stdin_stream::strip_trailing_cr(&mut display_path);
                    if display_path.first() == Some(&b'"') {
                        display_path = c_unquote_check_ignore_stdin(&display_path)?;
                    }
                }
                if process_path(display_path, stdout)? {
                    matched_any = true;
                }
                Ok(())
            },
        )?;
    } else {
        for display_path in path_args {
            if process_path(display_path, &mut stdout)? {
                matched_any = true;
            }
        }
    }
    stdout.flush()?;
    if matched_any {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

fn validate_check_ignore_pathspec(
    worktree_root: &Path,
    git_path: &[u8],
    display_path: &[u8],
    gitlink_paths: &[Vec<u8>],
) -> Result<()> {
    let mut absolute = worktree_root.to_path_buf();
    let mut components = git_path
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        absolute.push(String::from_utf8_lossy(component).as_ref());
        if fs::symlink_metadata(&absolute)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            eprintln!(
                "fatal: pathspec '{}' is beyond a symbolic link",
                String::from_utf8_lossy(display_path)
            );
            return Err(GitError::Exit(128));
        }
    }

    for gitlink in gitlink_paths {
        if git_path != gitlink
            && git_path
                .strip_prefix(gitlink.as_slice())
                .is_some_and(|rest| rest.first() == Some(&b'/'))
        {
            eprintln!(
                "fatal: Pathspec '{}' is in submodule '{}'",
                String::from_utf8_lossy(display_path),
                String::from_utf8_lossy(gitlink)
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

fn c_unquote_check_ignore_stdin(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut index = 1;
    while index < input.len() {
        match input[index] {
            b'"' if index + 1 == input.len() => return Ok(out),
            b'\\' if index + 1 < input.len() => {
                index += 1;
                out.push(input[index]);
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    eprintln!("fatal: line is badly quoted");
    Err(GitError::Exit(128))
}

fn write_check_ignore_quoted(stdout: &mut impl Write, path: &[u8]) -> Result<()> {
    let needs_quote = path
        .iter()
        .any(|byte| matches!(*byte, b'"' | b'\\' | b'\t' | b'\n'));
    if !needs_quote {
        stdout.write_all(path)?;
        return Ok(());
    }
    stdout.write_all(b"\"")?;
    for byte in path {
        match *byte {
            b'"' | b'\\' => {
                stdout.write_all(b"\\")?;
                stdout.write_all(&[*byte])?;
            }
            b'\t' => stdout.write_all(b"\\t")?,
            b'\n' => stdout.write_all(b"\\n")?,
            _ => stdout.write_all(&[*byte])?,
        }
    }
    stdout.write_all(b"\"")?;
    Ok(())
}

pub(crate) fn cmd_check_attr(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut read_stdin = false;
    let mut all = false;
    let mut z = false;
    let mut cached = false;
    let mut source = None::<String>;
    let mut before_separator = Vec::new();
    let mut path_args = Vec::new();
    let mut after_separator = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if after_separator {
            path_args.push(arg.as_bytes().to_vec());
            continue;
        }
        match arg.as_str() {
            "--" => after_separator = true,
            "--stdin" => read_stdin = true,
            "--no-stdin" => read_stdin = false,
            "--cached" => cached = true,
            "--no-cached" => cached = false,
            "--source" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("check-attr --source requires a value".into())
                })?;
                source = Some(value.clone());
            }
            "--no-source" => source = None,
            value if value.starts_with("--source=") => {
                let value = value.strip_prefix("--source=").ok_or_else(|| {
                    GitError::Command("check-attr --source requires a value".into())
                })?;
                source = Some(value.to_string());
            }
            "--all" | "-a" => all = true,
            "--no-all" => all = false,
            "-z" => z = true,
            value
                if value.starts_with('-')
                    && !value.starts_with("--")
                    && value.len() > 2
                    && value[1..].chars().all(|flag| matches!(flag, 'a' | 'z')) =>
            {
                for flag in value[1..].chars() {
                    match flag {
                        'a' => all = true,
                        'z' => z = true,
                        _ => {}
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "check-attr currently supports --all, --stdin, --cached, --no-source, -z, and path arguments; unsupported option {value}"
                )));
            }
            _ => before_separator.push(arg.as_bytes().to_vec()),
        }
    }
    let requested = if all {
        path_args.extend(before_separator);
        Vec::new()
    } else if read_stdin || after_separator {
        before_separator
    } else if before_separator.len() >= 2 {
        path_args.extend(before_separator.iter().skip(1).cloned());
        vec![before_separator[0].clone()]
    } else {
        before_separator
    };
    if !all && requested.is_empty() {
        return Err(GitError::Command(
            "check-attr requires --all or at least one attribute".into(),
        ));
    }
    if requested.iter().any(|attribute| attribute.is_empty()) {
        return Err(GitError::Command("check-attr: empty attribute name".into()));
    }
    if read_stdin && !path_args.is_empty() {
        return Err(GitError::Command(
            "check-attr --stdin cannot be combined with path arguments".into(),
        ));
    }
    if !read_stdin && path_args.is_empty() {
        return Err(GitError::Command(
            "check-attr requires path arguments or --stdin".into(),
        ));
    }

    let repo = RepositoryContext::from_session(cli_session)?;
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let worktree_root = repo
        .worktree_root()
        .ok()
        .map(Path::to_path_buf)
        .or(sley_worktree::worktree_root_for_git_dir(git_dir)?);
    let attr_root = worktree_root.as_deref().unwrap_or(git_dir);
    let prefix = if worktree_root.is_some() {
        worktree_prefix(cli_session, cwd, git_dir).unwrap_or_default()
    } else {
        String::new()
    };
    let attr_source = source
        .or_else(|| global_attr_source(cli_session))
        .or_else(|| std::env::var("GIT_ATTR_SOURCE").ok());
    let attr_tree_config = repo.config().get("attr", None, "tree").map(str::to_string);
    let source_tree = if let Some(source) = attr_source.as_deref() {
        let Some(oid) = resolve_attr_source_or_die(&repo, source, false)? else {
            unreachable!("explicit attr source resolution either returns an oid or exits");
        };
        Some(sley_rev::peel_to_tree(repo.objects(), format, &oid)?)
    } else if let Some(source) = attr_tree_config.as_deref() {
        match resolve_attr_source_or_die(&repo, source, true)? {
            Some(oid) => Some(sley_rev::peel_to_tree(repo.objects(), format, &oid)?),
            None => None,
        }
    } else {
        None
    };
    let mut stdout = io::stdout().lock();
    let terminator = if z { b'\0' } else { b'\n' };
    let process_path =
        |display_path: Vec<u8>, mut stdout: &mut std::io::StdoutLock<'_>| -> Result<()> {
            let path_arg = String::from_utf8_lossy(&display_path);
            let git_path = normalize_ls_files_pathspec(prefix.as_bytes(), &path_arg)?;
            let mut checks = if cached {
                sley_worktree::standard_attributes_for_path_from_index(
                    attr_root, git_dir, format, &git_path, &requested, all,
                )?
            } else if let Some(tree_oid) = source_tree.as_ref() {
                sley_worktree::standard_attributes_for_path_from_tree(
                    attr_root,
                    git_dir,
                    repo.objects(),
                    format,
                    tree_oid,
                    &git_path,
                    &requested,
                    all,
                )?
            } else {
                sley_worktree::standard_attributes_for_path_in_repo(
                    attr_root,
                    git_dir,
                    &git_path,
                    &requested,
                    all,
                    worktree_root.is_some(),
                    repo.config()
                        .get_bool("core", None, "ignorecase")
                        .unwrap_or(false),
                )?
            };
            if !all {
                fill_builtin_object_mode(
                    &mut checks,
                    worktree_root.as_deref(),
                    git_dir,
                    format,
                    &git_path,
                    cached,
                )?;
            }
            for check in checks {
                if z {
                    stdout.write_all(&display_path)?;
                    stdout.write_all(&[0])?;
                    stdout.write_all(&check.attribute)?;
                    stdout.write_all(&[0])?;
                    write_check_attr_state(&mut stdout, check.state.as_ref())?;
                    stdout.write_all(&[0])?;
                } else {
                    stdout.write_all(status_quote_path(&display_path, false).as_bytes())?;
                    stdout.write_all(b": ")?;
                    stdout.write_all(&check.attribute)?;
                    stdout.write_all(b": ")?;
                    write_check_attr_state(&mut stdout, check.state.as_ref())?;
                    stdout.write_all(b"\n")?;
                }
            }
            Ok(())
        };
    if read_stdin {
        crate::commands::stdin_stream::stream_stdin_records(
            terminator,
            &mut stdout,
            |mut display_path, stdout| {
                if display_path.is_empty() {
                    return Ok(());
                }
                if !z {
                    crate::commands::stdin_stream::strip_trailing_cr(&mut display_path);
                }
                process_path(display_path, stdout)
            },
        )?;
    } else {
        for display_path in path_args {
            process_path(display_path, &mut stdout)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn resolve_attr_source_or_die(
    repo: &RepositoryContext,
    source: &str,
    ignore_bad: bool,
) -> Result<Option<sley_core::ObjectId>> {
    match repo.resolve_revision(source) {
        Ok(oid) => Ok(Some(oid)),
        Err(_) if ignore_bad => Ok(None),
        Err(_) => {
            eprintln!("fatal: bad --attr-source or GIT_ATTR_SOURCE");
            Err(GitError::Exit(128))
        }
    }
}

fn fill_builtin_object_mode(
    checks: &mut [sley_worktree::AttributeCheck],
    worktree_root: Option<&Path>,
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    path: &[u8],
    cached: bool,
) -> Result<()> {
    for check in checks {
        if check.attribute != b"builtin_objectmode" {
            continue;
        }
        check.state = builtin_object_mode(worktree_root, git_dir, format, path, cached)?
            .map(sley_worktree::AttributeState::Value);
    }
    Ok(())
}

fn builtin_object_mode(
    worktree_root: Option<&Path>,
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    path: &[u8],
    cached: bool,
) -> Result<Option<Vec<u8>>> {
    if cached {
        return cached_object_mode(git_dir, format, path);
    }
    let Some(worktree_root) = worktree_root else {
        return Ok(None);
    };
    let filesystem_path = worktree_root.join(repo_path_to_path_buf(path));
    let Ok(metadata) = fs::symlink_metadata(&filesystem_path) else {
        return Ok(None);
    };
    let mode = if metadata.file_type().is_symlink() {
        0o120000
    } else if metadata.is_dir() {
        if filesystem_path.join(".git").exists() {
            0o160000
        } else {
            0o040000
        }
    } else if is_executable(&metadata) {
        0o100755
    } else {
        0o100644
    };
    Ok(Some(format!("{mode:06o}").into_bytes()))
}

fn cached_object_mode(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    path: &[u8],
) -> Result<Option<Vec<u8>>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(None);
    }
    let index = sley_index::Index::parse(&fs::read(index_path)?, format)?;
    for entry in index.entries {
        if entry.path.as_bytes() == path
            && index_entry_stage(&entry) == 0
            && (tree_entry_object_type(entry.mode) == sley_object::ObjectType::Blob
                || tree_entry_object_type(entry.mode) == sley_object::ObjectType::Tree
                || sley_index::is_gitlink(entry.mode))
        {
            return Ok(Some(format!("{:06o}", entry.mode).into_bytes()));
        }
    }
    Ok(None)
}

fn index_entry_stage(entry: &sley_index::IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

fn repo_path_to_path_buf(path: &[u8]) -> PathBuf {
    path.split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}
