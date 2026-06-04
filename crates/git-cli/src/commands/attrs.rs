//! Attribute and ignore inspection commands (`check-attr`, `check-ignore`).

use std::collections::BTreeSet;
use std::env;
use std::io::{self, Read, Write};

use git_core::{GitError, Result};
use git_odb::FileObjectDatabase;

use crate::{
    check_ignore_tracked_paths, discover_git_dir, normalize_ls_files_pathspec,
    repository_object_format, resolve_cli_path, resolve_revision, worktree_prefix,
    worktree_root_for_git_dir, write_check_attr_state,
};

pub(crate) fn cmd_check_ignore(args: &[String]) -> Result<()> {
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
    if read_stdin && !path_args.is_empty() {
        return Err(GitError::Command(
            "check-ignore --stdin cannot be combined with path arguments".into(),
        ));
    }
    if quiet && verbose {
        eprintln!("fatal: cannot have both --quiet and --verbose");
        return Err(GitError::Exit(128));
    }
    if z && !read_stdin {
        eprintln!("fatal: -z only makes sense with --stdin");
        return Err(GitError::Exit(128));
    }
    if non_matching && !verbose {
        eprintln!("fatal: --non-matching is only valid with --verbose");
        return Err(GitError::Exit(128));
    }
    if read_stdin {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        let separator = if z { b'\0' } else { b'\n' };
        path_args.extend(
            input
                .split(|byte| *byte == separator)
                .filter(|path| !path.is_empty())
                .map(|path| path.strip_suffix(b"\r").unwrap_or(path).to_vec()),
        );
    }
    if path_args.is_empty() {
        return Err(GitError::Command(
            "check-ignore requires path arguments or --stdin".into(),
        ));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let prefix = worktree_prefix(&cwd, &git_dir)?;
    let tracked_paths = if no_index {
        BTreeSet::new()
    } else {
        check_ignore_tracked_paths(&git_dir, format)?
    };
    let mut stdout = io::stdout().lock();
    let terminator = if z { b'\0' } else { b'\n' };
    let mut matched_any = false;
    for display_path in path_args {
        let path_arg = String::from_utf8_lossy(&display_path);
        let git_path = normalize_ls_files_pathspec(prefix.as_bytes(), &path_arg)?;
        if tracked_paths.contains(&git_path) {
            continue;
        }
        let absolute = resolve_cli_path(&cwd, &path_arg);
        let ignore_match =
            git_worktree::standard_ignore_match(&worktree_root, &git_path, absolute.is_dir())?;
        if let Some(ignore_match) = ignore_match {
            if verbose || ignore_match.ignored {
                matched_any = true;
            }
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
                    stdout.write_all(&display_path)?;
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
                stdout.write_all(&display_path)?;
                stdout.write_all(&[terminator])?;
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

pub(crate) fn cmd_check_attr(args: &[String]) -> Result<()> {
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
    if read_stdin && !path_args.is_empty() {
        return Err(GitError::Command(
            "check-attr --stdin cannot be combined with path arguments".into(),
        ));
    }
    if read_stdin {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        let separator = if z { b'\0' } else { b'\n' };
        path_args.extend(
            input
                .split(|byte| *byte == separator)
                .filter(|path| !path.is_empty())
                .map(|path| path.strip_suffix(b"\r").unwrap_or(path).to_vec()),
        );
    }
    if path_args.is_empty() {
        return Err(GitError::Command(
            "check-attr requires path arguments or --stdin".into(),
        ));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let prefix = worktree_prefix(&cwd, &git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let source_tree = if let Some(source) = source.as_deref() {
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let oid = resolve_revision(&git_dir, format, source)?;
        let tree_oid = git_rev::peel_to_tree(&db, format, &oid)?;
        Some((db, format, tree_oid))
    } else {
        None
    };
    let mut stdout = io::stdout().lock();
    for display_path in path_args {
        let path_arg = String::from_utf8_lossy(&display_path);
        let git_path = normalize_ls_files_pathspec(prefix.as_bytes(), &path_arg)?;
        let checks = if cached {
            git_worktree::standard_attributes_for_path_from_index(
                &worktree_root,
                &git_dir,
                format,
                &git_path,
                &requested,
                all,
            )?
        } else if let Some((db, format, tree_oid)) = source_tree.as_ref() {
            git_worktree::standard_attributes_for_path_from_tree(
                &worktree_root,
                db,
                *format,
                tree_oid,
                &git_path,
                &requested,
                all,
            )?
        } else {
            git_worktree::standard_attributes_for_path(&worktree_root, &git_path, &requested, all)?
        };
        for check in checks {
            if z {
                stdout.write_all(&display_path)?;
                stdout.write_all(&[0])?;
                stdout.write_all(&check.attribute)?;
                stdout.write_all(&[0])?;
                write_check_attr_state(&mut stdout, check.state.as_ref())?;
                stdout.write_all(&[0])?;
            } else {
                stdout.write_all(&display_path)?;
                stdout.write_all(b": ")?;
                stdout.write_all(&check.attribute)?;
                stdout.write_all(b": ")?;
                write_check_attr_state(&mut stdout, check.state.as_ref())?;
                stdout.write_all(b"\n")?;
            }
        }
    }
    stdout.flush()?;
    Ok(())
}
