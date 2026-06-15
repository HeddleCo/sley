//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_options::{OptFlags, OptValue, OptionSpec, ParsedValue, UsageError, parse_options};

#[derive(Debug)]
struct ReflogShowOptions {
    reference: String,
    display: String,
    format: ReflogFormat,
    max_count: Option<usize>,
}

#[derive(Debug)]
enum ReflogFormat {
    Default,
    Message { final_newline: bool },
}

pub(crate) fn cmd_reflog(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "exists") {
        return cmd_reflog_exists(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "list") {
        return cmd_reflog_list(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "delete") {
        return cmd_reflog_delete(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "drop") {
        return cmd_reflog_drop(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "write") {
        return cmd_reflog_write(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "expire") {
        return cmd_reflog_expire(&args[1..]);
    }
    let options = parse_reflog_show_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut entries = store.read_reflog(&options.reference)?;
    entries.reverse();
    // `git reflog show` (== `git log -g`) walks the reflog newest-to-oldest and
    // prints EVERY entry verbatim: HEAD@{N} is just the entry's position. There
    // is no reachability filter and no dedup by OID — a no-op `rebase --no-ff`
    // over an up-to-date branch records same-OID start/pick/finish entries and
    // upstream shows them all (that growth is exactly what t3432 asserts:
    // "--no-ff ... is work"). A prior implementation filtered by reachability
    // and deduped consecutive OIDs; that matched upstream only for the
    // pathological non-monotonic reflog (entries whose timestamps run backwards,
    // which never occurs in real usage) and dropped the legitimate entries a
    // real, monotonic reflog accumulates.
    let mut selected = Vec::new();
    for entry in &entries {
        if options
            .max_count
            .is_some_and(|max_count| selected.len() >= max_count)
        {
            break;
        }
        selected.push(entry);
    }
    for (shown, entry) in selected.iter().enumerate() {
        match options.format {
            ReflogFormat::Default => println!(
                "{} {}@{{{}}}: {}",
                format_log_abbrev_oid(&entry.new_oid),
                options.display,
                shown,
                String::from_utf8_lossy(&entry.message)
            ),
            ReflogFormat::Message { final_newline } => {
                if final_newline || shown + 1 < selected.len() {
                    println!("{}", String::from_utf8_lossy(&entry.message));
                } else {
                    print!("{}", String::from_utf8_lossy(&entry.message));
                }
            }
        }
    }
    Ok(())
}

fn cmd_reflog_exists(args: &[String]) -> Result<()> {
    let Some(reference) = args.first() else {
        eprintln!("usage: git reflog exists <ref>");
        eprintln!();
        return Err(GitError::Exit(129));
    };
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    if reflog_path_for_ref(&git_dir, reference)?.is_file() {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

fn cmd_reflog_list(args: &[String]) -> Result<()> {
    let mut refs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            refs.extend(args[index + 1..].iter().cloned());
            break;
        }
        if arg.starts_with('-') {
            eprintln!("error: unknown option `{}'", arg.trim_start_matches('-'));
            eprintln!("usage: git reflog list");
            eprintln!();
            return Err(GitError::Exit(129));
        }
        refs.push(arg.clone());
        index += 1;
    }
    if let Some(reference) = refs.first() {
        eprintln!("error: list does not accept arguments: '{reference}'");
        return Err(GitError::Exit(255));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let mut names = BTreeSet::new();
    collect_repository_reflog_names(&git_dir, &mut names)?;
    for name in names {
        println!("{name}");
    }
    Ok(())
}

fn collect_reflog_names(path: &Path, base: &Path, names: &mut BTreeSet<String>) -> Result<()> {
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_reflog_names(&path, base, names)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(base)
                .map_err(|err| GitError::InvalidPath(err.to_string()))?;
            names.insert(
                relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
            );
        }
    }
    Ok(())
}

fn collect_repository_reflog_names(git_dir: &Path, names: &mut BTreeSet<String>) -> Result<()> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let common_logs = common_git_dir.join("logs");
    collect_reflog_names(&common_logs, &common_logs, names)?;

    let worktree_logs = git_dir.join("logs");
    if worktree_logs != common_logs {
        collect_reflog_names(&worktree_logs, &worktree_logs, names)?;
    }
    Ok(())
}

fn reflog_path_for_ref(git_dir: &Path, name: &str) -> Result<PathBuf> {
    Ok(reflog_logs_dir_for_ref(git_dir, name)?.join(name))
}

fn loose_ref_path_for_ref(git_dir: &Path, name: &str) -> Result<PathBuf> {
    if name == "HEAD" {
        Ok(git_dir.join(name))
    } else {
        Ok(common_git_dir_for_git_dir(git_dir)?.join(name))
    }
}

fn lock_path_for_loose_ref_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| GitError::InvalidPath("ref path has no file name".into()))?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(path.with_file_name(lock_name))
}

fn reflog_logs_dir_for_ref(git_dir: &Path, name: &str) -> Result<PathBuf> {
    if name == "HEAD" {
        Ok(git_dir.join("logs"))
    } else {
        Ok(common_git_dir_for_git_dir(git_dir)?.join("logs"))
    }
}

#[derive(Debug, Clone, Copy)]
struct ReflogDeleteOptions {
    dry_run: bool,
    verbose: bool,
    update_ref: bool,
}

#[derive(Debug, Clone, Copy)]
struct ReflogExpireOptions {
    dry_run: bool,
    verbose: bool,
    rewrite: bool,
    update_ref: bool,
    all: bool,
    expire: i64,
    expire_unreachable: i64,
}

#[derive(Debug, Clone, Copy)]
struct ReflogDropOptions {
    all: bool,
}

fn cmd_reflog_delete(args: &[String]) -> Result<()> {
    let mut options = ReflogDeleteOptions {
        dry_run: false,
        verbose: false,
        update_ref: false,
    };
    let mut specs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--" => {
                specs.extend(args[index + 1..].iter().cloned());
                break;
            }
            "-n" | "--dry-run" => options.dry_run = true,
            "--no-dry-run" => options.dry_run = false,
            "--verbose" => options.verbose = true,
            "--no-verbose" => options.verbose = false,
            "--updateref" => options.update_ref = true,
            "--no-updateref" => options.update_ref = false,
            "--rewrite" | "--no-rewrite" => {}
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return reflog_delete_usage();
            }
            value => specs.push(value.to_string()),
        }
        index += 1;
    }
    if specs.is_empty() {
        eprintln!("error: no reflog specified to delete");
        return Err(GitError::Exit(255));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut exit_code = 0;
    for spec in specs {
        if let Err(GitError::Exit(code)) = delete_reflog_entry(&store, &spec, options) {
            exit_code = code;
        }
    }
    if exit_code == 0 {
        Ok(())
    } else {
        Err(GitError::Exit(exit_code))
    }
}

fn reflog_delete_usage<T>() -> Result<T> {
    eprintln!("usage: git reflog delete [--rewrite] [--updateref]");
    eprintln!("                         [--dry-run | -n] [--verbose] <ref>@{{<specifier>}}...");
    eprintln!();
    eprintln!("    -n, --[no-]dry-run    do not actually prune any entries");
    eprintln!(
        "    --[no-]rewrite        rewrite the old SHA1 with the new SHA1 of the entry that now precedes it"
    );
    eprintln!(
        "    --[no-]updateref      update the reference to the value of the top reflog entry"
    );
    eprintln!("    --[no-]verbose        print extra information on screen");
    eprintln!();
    Err(GitError::Exit(129))
}

fn delete_reflog_entry(
    store: &FileRefStore,
    spec: &str,
    options: ReflogDeleteOptions,
) -> Result<()> {
    let Some((reference, selector)) = parse_reflog_delete_spec(spec) else {
        eprintln!("error: not a reflog: {spec}");
        return Err(GitError::Exit(255));
    };
    let mut entries = store.read_reflog(&reference)?;
    if entries.is_empty() {
        eprintln!("error: no reflog for '{spec}'");
        return Err(GitError::Exit(255));
    }
    let Some(delete_index) = entries.len().checked_sub(selector + 1) else {
        return Ok(());
    };
    if options.verbose {
        for (index, entry) in entries.iter().enumerate() {
            let action = if index == delete_index {
                "prune"
            } else {
                "keep"
            };
            println!("{action} {}", String::from_utf8_lossy(&entry.message));
        }
    }
    if !options.dry_run {
        let old_tip = entries.last().map(|entry| entry.new_oid);
        entries.remove(delete_index);
        let new_tip = entries.last().map(|entry| entry.new_oid);
        store.write_reflog(&reference, &entries)?;
        if options.update_ref
            && reference != "HEAD"
            && let (Some(old_tip), Some(new_tip)) = (old_tip, new_tip)
        {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: reference,
                expected: Some(RefTarget::Direct(old_tip)),
                new: RefTarget::Direct(new_tip),
                reflog: None,
            });
            tx.commit()?;
        }
    }
    Ok(())
}

fn parse_reflog_delete_spec(spec: &str) -> Option<(String, usize)> {
    let spec = spec.strip_suffix('}')?;
    let (reference, selector) = spec.rsplit_once("@{")?;
    let selector = selector.parse::<usize>().ok()?;
    let reference = reflog_reference_name(Some(reference)).ok()?;
    Some((reference, selector))
}

fn cmd_reflog_drop(args: &[String]) -> Result<()> {
    let mut options = ReflogDropOptions { all: false };
    let mut refs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--" => {
                refs.extend(args[index + 1..].iter().cloned());
                break;
            }
            "--all" => options.all = true,
            "--no-all" => options.all = false,
            "--single-worktree" | "--no-single-worktree" => {}
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return reflog_drop_usage();
            }
            value => refs.push(value.to_string()),
        }
        index += 1;
    }
    if options.all && !refs.is_empty() {
        eprintln!("usage: references specified along with --all");
        return Err(GitError::Exit(129));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let logs_dir = common_git_dir.join("logs");
    if options.all {
        if logs_dir.exists() {
            fs::remove_dir_all(&logs_dir)?;
        }
        let worktree_logs_dir = git_dir.join("logs");
        if worktree_logs_dir != logs_dir && worktree_logs_dir.exists() {
            fs::remove_dir_all(worktree_logs_dir)?;
        }
        return Ok(());
    }
    let mut exit_code = 0;
    for reference in refs {
        let display = reference.clone();
        let reference = reflog_reference_name(Some(&reference))?;
        let path = reflog_path_for_ref(&git_dir, &reference)?;
        let logs_dir = reflog_logs_dir_for_ref(&git_dir, &reference)?;
        if !path.is_file() {
            eprintln!("error: reflog could not be found: '{display}'");
            exit_code = 255;
            continue;
        }
        fs::remove_file(&path)?;
        prune_empty_reflog_dirs(path.parent(), &logs_dir)?;
    }
    if exit_code == 0 {
        Ok(())
    } else {
        Err(GitError::Exit(exit_code))
    }
}

fn prune_empty_reflog_dirs(mut path: Option<&Path>, logs_dir: &Path) -> Result<()> {
    while let Some(current) = path {
        if current == logs_dir {
            break;
        }
        match fs::remove_dir(current) {
            Ok(()) => path = current.parent(),
            Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn reflog_drop_usage<T>() -> Result<T> {
    eprintln!("usage: git reflog drop [--all [--single-worktree] | <refs>...]");
    eprintln!();
    eprintln!("    --[no-]all            drop the reflogs of all references");
    eprintln!("    --[no-]single-worktree");
    eprintln!("                          drop reflogs from the current worktree only");
    eprintln!();
    Err(GitError::Exit(129))
}

fn cmd_reflog_write(args: &[String]) -> Result<()> {
    if args.len() != 4 {
        eprintln!("usage: git reflog write <ref> <old-oid> <new-oid> <message>");
        eprintln!();
        return Err(GitError::Exit(129));
    }
    let reference = &args[0];
    if validate_ref_name(reference).is_err() {
        eprintln!("fatal: invalid reference name: {reference}");
        return Err(GitError::Exit(128));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let old_oid = parse_reflog_write_oid(format, &args[1], "old")?;
    let new_oid = parse_reflog_write_oid(format, &args[2], "new")?;
    let zero = zero_oid(format)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    validate_reflog_write_object(&db, &old_oid, &zero, "old")?;
    validate_reflog_write_object(&db, &new_oid, &zero, "new")?;

    let store = FileRefStore::new(&git_dir, format);
    store.append_reflog(
        reference,
        &ReflogEntry {
            old_oid,
            new_oid,
            committer: commit_identity_from_env("COMMITTER")?,
            message: args[3].as_bytes().to_vec(),
        },
    )
}

fn parse_reflog_write_oid(format: ObjectFormat, value: &str, role: &str) -> Result<ObjectId> {
    ObjectId::from_hex(format, value).map_err(|_| {
        eprintln!("fatal: invalid {role} object ID: '{value}'");
        GitError::Exit(128)
    })
}

fn validate_reflog_write_object(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    zero: &ObjectId,
    role: &str,
) -> Result<()> {
    if oid == zero || db.read_object(oid).is_ok() {
        return Ok(());
    }
    eprintln!("fatal: {role} object '{oid}' does not exist");
    Err(GitError::Exit(128))
}

fn cmd_reflog_expire(args: &[String]) -> Result<()> {
    let mut options = ReflogExpireOptions {
        dry_run: false,
        verbose: false,
        rewrite: false,
        update_ref: false,
        all: false,
        expire: current_unix_seconds().saturating_sub(90 * 24 * 60 * 60),
        expire_unreachable: current_unix_seconds().saturating_sub(30 * 24 * 60 * 60),
    };
    let mut refs = Vec::new();
    let mut args = GitArgCursor::new(args);
    while let Some(arg) = args.next() {
        match arg {
            "--" => {
                refs.extend(args.rest().iter().cloned());
                break;
            }
            "-n" | "--dry-run" => options.dry_run = true,
            "--no-dry-run" => options.dry_run = false,
            "--verbose" => options.verbose = true,
            "--no-verbose" => options.verbose = false,
            "--rewrite" => options.rewrite = true,
            "--no-rewrite" => options.rewrite = false,
            "--updateref" => options.update_ref = true,
            "--no-updateref" => options.update_ref = false,
            "--stale-fix" | "--no-stale-fix" => {}
            "--all" => options.all = true,
            "--no-all" => options.all = false,
            "--single-worktree" | "--no-single-worktree" => {}
            "--expire" | "--expire-unreachable" => {
                let Some(value) = args.next_value() else {
                    return reflog_expire_option_requires_value(arg.trim_start_matches("--"));
                };
                let cutoff = parse_reflog_expire_time(value, arg)?;
                if arg == "--expire" {
                    options.expire = cutoff;
                } else {
                    options.expire_unreachable = cutoff;
                }
            }
            value if let Some(time) = long_option_value(value, "expire") => {
                options.expire = parse_reflog_expire_time(time, "--expire")?;
            }
            value if let Some(time) = long_option_value(value, "expire-unreachable") => {
                options.expire_unreachable =
                    parse_reflog_expire_time(time, "--expire-unreachable")?;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return reflog_expire_usage();
            }
            value => refs.push(value.to_string()),
        }
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut targets = BTreeSet::new();
    if options.all {
        collect_repository_reflog_names(&git_dir, &mut targets)?;
    }
    for reference in refs {
        targets.insert(reflog_reference_name(Some(&reference))?);
    }
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let mut exit_code = 0;
    for reference in targets {
        if let Err(GitError::Exit(code)) =
            expire_reflog_entries(&store, &db, &git_dir, format, &reference, options)
        {
            exit_code = code;
        }
    }
    if exit_code == 0 {
        Ok(())
    } else {
        Err(GitError::Exit(exit_code))
    }
}

fn expire_reflog_entries(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    reference: &str,
    options: ReflogExpireOptions,
) -> Result<()> {
    let mut entries = store.read_reflog(reference)?;
    if entries.is_empty() {
        eprintln!("error: reflog could not be found: '{reference}'");
        return Err(GitError::Exit(255));
    }
    let reachable = resolve_revision(git_dir, format, reference)
        .ok()
        .and_then(|tip| ancestor_depths(db, format, &tip).ok());
    let zero = zero_oid(format)?;
    let mut retained = Vec::new();
    for entry in &entries {
        let timestamp = entry.timestamp_seconds()?;
        let reachable_from_tip = reachable.as_ref().is_some_and(|commits| {
            commits.contains_key(&entry.new_oid)
                && (entry.old_oid == zero || commits.contains_key(&entry.old_oid))
        });
        let prune = timestamp < options.expire
            || (!reachable_from_tip && timestamp < options.expire_unreachable);
        if options.verbose {
            let action = if prune { "prune" } else { "keep" };
            println!("{action} {}", String::from_utf8_lossy(&entry.message));
        }
        if !prune {
            retained.push(entry.clone());
        }
    }
    if options.dry_run {
        return Ok(());
    }
    let old_tip = entries.last().map(|entry| entry.new_oid);
    if options.rewrite {
        let mut previous = None;
        for entry in &mut retained {
            entry.old_oid = previous.unwrap_or_else(|| zero.clone());
            previous = Some(entry.new_oid);
        }
    }
    entries = retained;
    let new_tip = entries.last().map(|entry| entry.new_oid);
    store.write_reflog(reference, &entries)?;
    if options.update_ref
        && reference != "HEAD"
        && let (Some(old_tip), Some(new_tip)) = (old_tip, new_tip)
        && old_tip != new_tip
    {
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: reference.to_string(),
            expected: Some(RefTarget::Direct(old_tip)),
            new: RefTarget::Direct(new_tip),
            reflog: None,
        });
        tx.commit()?;
    }
    Ok(())
}

fn reflog_expire_option_requires_value<T>(option: &str) -> Result<T> {
    eprintln!("error: option `{option}' requires a value");
    Err(GitError::Exit(129))
}

fn reflog_expire_usage<T>() -> Result<T> {
    eprintln!("usage: git reflog expire [--expire=<time>] [--expire-unreachable=<time>]");
    eprintln!("                         [--rewrite] [--updateref] [--stale-fix]");
    eprintln!(
        "                         [--dry-run | -n] [--verbose] [--all [--single-worktree] | <refs>...]"
    );
    eprintln!();
    eprintln!("    -n, --[no-]dry-run    do not actually prune any entries");
    eprintln!(
        "    --[no-]rewrite        rewrite the old SHA1 with the new SHA1 of the entry that now precedes it"
    );
    eprintln!(
        "    --[no-]updateref      update the reference to the value of the top reflog entry"
    );
    eprintln!("    --[no-]verbose        print extra information on screen");
    eprintln!("    --expire <timestamp>  prune entries older than the specified time");
    eprintln!("    --expire-unreachable <timestamp>");
    eprintln!(
        "                          prune entries older than <time> that are not reachable from the current tip of the branch"
    );
    eprintln!("    --[no-]stale-fix      prune any reflog entries that point to broken commits");
    eprintln!("    --[no-]all            process the reflogs of all references");
    eprintln!("    --[no-]single-worktree");
    eprintln!(
        "                          limits processing to reflogs from the current worktree only"
    );
    eprintln!();
    Err(GitError::Exit(129))
}

fn parse_reflog_show_options(args: &[String]) -> Result<ReflogShowOptions> {
    let mut args = args;
    if args.first().is_some_and(|arg| arg == "show") {
        args = &args[1..];
    }
    let mut format = ReflogFormat::Default;
    let mut max_count = None;
    let mut refs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--" => {
                refs.extend(args[index + 1..].iter().cloned());
                break;
            }
            "--oneline" => format = ReflogFormat::Default,
            "--format=%gs" | "--pretty=%gs" => {
                format = ReflogFormat::Message {
                    final_newline: true,
                };
            }
            "--pretty=format:%gs" | "--format=format:%gs" => {
                format = ReflogFormat::Message {
                    final_newline: false,
                };
            }
            "--format" | "--pretty" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Command(format!("{arg} requires a value")));
                };
                match value.as_str() {
                    "%gs" => {
                        format = ReflogFormat::Message {
                            final_newline: true,
                        };
                    }
                    "format:%gs" => {
                        format = ReflogFormat::Message {
                            final_newline: false,
                        };
                    }
                    "oneline" => format = ReflogFormat::Default,
                    _ => {
                        return Err(GitError::Unsupported(
                            "reflog currently supports only --format=%gs".into(),
                        ));
                    }
                }
            }
            value if let Some(count) = value.strip_prefix("--max-count=") => {
                max_count = Some(parse_reflog_count(count)?);
            }
            "--max-count" | "-n" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Command(format!("{arg} requires a value")));
                };
                max_count = Some(parse_reflog_count(value)?);
            }
            value if value.starts_with("-n") && value.len() > 2 => {
                max_count = Some(parse_reflog_count(&value[2..])?);
            }
            value
                if value.starts_with('-')
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                max_count = Some(parse_reflog_count(&value[1..])?);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Unsupported(format!(
                    "unsupported reflog option {value}"
                )));
            }
            value => refs.push(value.to_string()),
        }
        index += 1;
    }
    if refs.len() > 1 {
        return Err(GitError::Command(
            "reflog show currently accepts at most one ref".into(),
        ));
    }
    let display = refs.first().cloned().unwrap_or_else(|| "HEAD".to_string());
    let reference = reflog_reference_name(refs.first().map(String::as_str))?;
    Ok(ReflogShowOptions {
        reference,
        display,
        format,
        max_count,
    })
}

pub(crate) fn cmd_update_server_info(args: &[String]) -> Result<()> {
    parse_update_server_info_options(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);

    let info_dir = common_git_dir.join("info");
    fs::create_dir_all(&info_dir)?;
    fs::write(
        info_dir.join("refs"),
        update_server_info_refs(&store, &db, format)?,
    )?;

    let objects_info_dir = repository_objects_dir(&common_git_dir).join("info");
    fs::create_dir_all(&objects_info_dir)?;
    fs::write(
        objects_info_dir.join("packs"),
        update_server_info_packs(
            &repository_objects_dir(&common_git_dir).join("pack"),
            format,
        )?,
    )?;
    Ok(())
}

fn parse_update_server_info_options(args: &[String]) -> Result<()> {
    let mut after_delimiter = false;
    for arg in args {
        if after_delimiter {
            return update_server_info_usage();
        }
        match arg.as_str() {
            "-f" | "--force" | "--no-force" => {}
            "--" => after_delimiter = true,
            value if value.starts_with("--force=") => {
                eprintln!("error: option `force' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--no-force=") => {
                eprintln!("error: option `no-force' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return update_server_info_usage();
            }
            value if value.starts_with('-') && value.len() > 1 => {
                for option in value[1..].chars() {
                    if option != 'f' {
                        eprintln!("error: unknown switch `{option}'");
                        return update_server_info_usage();
                    }
                }
            }
            _ => return update_server_info_usage(),
        }
    }
    Ok(())
}

fn update_server_info_refs(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<Vec<u8>> {
    let refs = store.list_refs()?;
    let mut out = Vec::with_capacity(refs.len() * (format.hex_len() + 32));
    for reference in refs {
        let Some(oid) = resolve_ref_to_oid(store, &reference.name)? else {
            continue;
        };
        update_server_info_refs_line(&mut out, &oid, &reference.name);
        if let Some(peeled) = pack_refs_peeled_oid(db, format, &oid)? {
            update_server_info_refs_line(&mut out, &peeled, &format!("{}^{{}}", reference.name));
        }
    }
    Ok(out)
}

fn update_server_info_refs_line(out: &mut Vec<u8>, oid: &ObjectId, name: &str) {
    out.extend_from_slice(oid.to_hex().as_bytes());
    out.push(b'\t');
    out.extend_from_slice(name.as_bytes());
    out.push(b'\n');
}

fn update_server_info_packs(pack_dir: &Path, format: ObjectFormat) -> Result<Vec<u8>> {
    let mut packs = Vec::new();
    if pack_dir.exists() {
        for entry in fs::read_dir(pack_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(name) = update_server_info_pack_name(&entry.path(), format) {
                packs.push(name);
            }
        }
    }
    packs.sort();

    let mut out = Vec::with_capacity(packs.len() * (format.hex_len() + 9));
    for name in packs {
        out.extend_from_slice(b"P ");
        out.extend_from_slice(name.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
    Ok(out)
}

fn update_server_info_pack_name(path: &Path, format: ObjectFormat) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let hash = name.strip_prefix("pack-")?.strip_suffix(".pack")?;
    if hash.len() == format.hex_len()
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        && path.with_extension("idx").is_file()
    {
        Some(name.to_string())
    } else {
        None
    }
}

fn update_server_info_usage<T>() -> Result<T> {
    eprintln!("usage: git update-server-info [-f | --force]");
    eprintln!();
    eprintln!("    -f, --[no-]force      update the info files from scratch");
    eprintln!();
    Err(GitError::Exit(129))
}

pub(crate) fn cmd_update_ref(args: &[String]) -> Result<()> {
    let mut message = b"update by sley".to_vec();
    let mut delete = false;
    let mut create_reflog = false;
    let mut deref = true;
    let mut stdin = false;
    let mut nul = false;
    let mut batch_updates = false;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    let mut positional_only = false;
    while let Some(arg) = iter.next() {
        if positional_only {
            positional.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-d" | "--delete" => delete = true,
            "--stdin" => stdin = true,
            "--no-stdin" => stdin = false,
            "-z" => nul = true,
            "-0" | "--batch-updates" => batch_updates = true,
            "--no-batch-updates" => batch_updates = false,
            "--deref" => deref = true,
            "--no-deref" => deref = false,
            "--create-reflog" => create_reflog = true,
            "--no-create-reflog" => create_reflog = false,
            "-m" => {
                message = iter
                    .next()
                    .ok_or_else(|| GitError::Command("-m requires a reflog message".into()))?
                    .as_bytes()
                    .to_vec();
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                message = value.as_bytes()[2..].to_vec();
            }
            value => positional.push(value.to_string()),
        }
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    if stdin {
        if delete || !positional.is_empty() {
            return Err(GitError::Command(
                "update-ref --stdin does not accept command-line refs".into(),
            ));
        }
        let stdin_context = UpdateRefStdinContext {
            git_dir: &git_dir,
            store: &store,
            format,
            create_reflog,
            batch_updates,
            message,
        };
        return update_ref_stdin(stdin_context, deref, nul);
    }
    if batch_updates {
        eprintln!("fatal: --batch-updates can only be used with --stdin");
        return Err(GitError::Exit(128));
    }
    if nul {
        return update_ref_usage();
    }
    if delete {
        if positional.len() != 1 && positional.len() != 2 {
            return Err(GitError::Command(
                "update-ref -d requires <ref> [<old-oid>]".into(),
            ));
        }
        let expected_oid = if let Some(old) = positional.get(1) {
            Some(parse_update_ref_expected(&git_dir, format, &store, old)?)
        } else {
            None
        };
        let name = update_ref_effective_name(&store, &positional[0], deref)?;
        return update_ref_delete(&store, format, &name, expected_oid.as_ref());
    }
    if positional.len() != 2 && positional.len() != 3 {
        return Err(GitError::Command(
            "update-ref requires <ref> <new-oid> [<old-oid>] or -d <ref>".into(),
        ));
    }
    let requested_name = positional[0].clone();
    let name = update_ref_effective_name(&store, &requested_name, deref)?;
    let new_oid = parse_update_ref_new_oid(&git_dir, format, &store, &positional[1])?;
    let expected_oid = if let Some(old) = positional.get(2) {
        Some(parse_update_ref_expected(&git_dir, format, &store, old)?)
    } else {
        None
    };
    check_update_ref_new_value(&git_dir, format, &name, &new_oid).map_err(|reason| {
        eprintln!(
            "fatal: update_ref failed for ref '{requested_name}': cannot update ref '{name}': {reason}"
        );
        GitError::Exit(128)
    })?;
    let current = store.read_ref(&name)?;
    if let Some(expected_oid) = expected_oid.as_ref() {
        check_update_ref_expected(format, &name, current.as_ref(), expected_oid)?;
    }
    if new_oid == zero_oid(format)? {
        return update_ref_delete(&store, format, &name, None);
    }
    let old_oid = match current {
        Some(RefTarget::Direct(oid)) => oid,
        _ => zero_oid(format)?,
    };
    let reflog =
        update_ref_should_write_reflog(&git_dir, &name, create_reflog)?.then(|| ReflogEntry {
            old_oid,
            new_oid,
            committer: default_committer(),
            message,
        });
    let tx_name = name.clone();
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name,
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog,
    });
    match tx.commit() {
        Ok(()) => Ok(()),
        Err(GitError::Io(message))
            if message.starts_with(&format!("could not lock ref {tx_name}: ")) =>
        {
            let prefix = format!("could not lock ref {tx_name}: ");
            update_ref_lock_failure(&tx_name, message.trim_start_matches(&prefix))
        }
        Err(err) => Err(err),
    }
}

fn update_ref_usage() -> Result<()> {
    eprintln!(
        "usage: git update-ref [<options>] -d <refname> [<old-oid>]\n   or: git update-ref [<options>]    <refname> <new-oid> [<old-oid>]\n   or: git update-ref [<options>] --stdin [-z] [--batch-updates]\n\n    -m <reason>           reason of the update\n    -d                    delete the reference\n    --no-deref            update <refname> not the one it points to\n    --deref               opposite of --no-deref\n    -z                    stdin has NUL-terminated arguments\n    --[no-]stdin          read updates from stdin\n    --[no-]create-reflog  create a reflog\n    -0, --[no-]batch-updates\n                          batch reference updates"
    );
    Err(GitError::Exit(129))
}

struct UpdateRefStdinContext<'a> {
    git_dir: &'a Path,
    store: &'a FileRefStore,
    format: ObjectFormat,
    create_reflog: bool,
    message: Vec<u8>,
    batch_updates: bool,
}

struct UpdateRefStdinWriteRequest<'a> {
    name: String,
    new_oid: ObjectId,
    expected_oid: Option<&'a ObjectId>,
}

/// The three states git distinguishes for an old-value (`<old-oid>`) field
/// parsed with PARSE_SHA1_OLD (no ALLOW_EMPTY): absent (no check), present but
/// empty — which git treats as the all-zeros OID (`have_old = 1`), or a
/// concrete value.
enum OldOid {
    Absent,
    Zero,
    Value(String),
}

/// The dispatch table for `update-ref --stdin`, mirroring git's
/// `static const struct parse_cmd command[]` (builtin/update-ref.c). `args` is
/// the number of NUL-terminated records the command consumes under `-z` (the
/// command record itself plus its fixed arguments); it controls how many extra
/// records the `-z` driver pre-reads before dispatch.
struct RefStdinCommand {
    prefix: &'static str,
    /// Number of records (including the `<cmd> <ref>` record) the `-z` driver
    /// stitches before handing the command to the dispatcher.
    args: usize,
}

const REF_STDIN_COMMANDS: &[RefStdinCommand] = &[
    RefStdinCommand { prefix: "update", args: 3 },
    RefStdinCommand { prefix: "create", args: 2 },
    RefStdinCommand { prefix: "delete", args: 2 },
    RefStdinCommand { prefix: "verify", args: 2 },
    RefStdinCommand { prefix: "symref-update", args: 4 },
    RefStdinCommand { prefix: "symref-create", args: 2 },
    RefStdinCommand { prefix: "symref-delete", args: 2 },
    RefStdinCommand { prefix: "symref-verify", args: 2 },
    RefStdinCommand { prefix: "option", args: 1 },
    RefStdinCommand { prefix: "start", args: 0 },
    RefStdinCommand { prefix: "prepare", args: 0 },
    RefStdinCommand { prefix: "abort", args: 0 },
    RefStdinCommand { prefix: "commit", args: 0 },
];

/// Match a command verb against the dispatch table the way git does: the input
/// must start with the prefix, and the byte immediately after the prefix must
/// be the expected separator — `SP` (byte 0x20) when the command takes
/// arguments, or the record terminator when it does not. Returns the matched
/// command and the byte offset at which its arguments begin.
fn match_ref_stdin_command(input: &[u8], term: u8) -> Option<(&'static RefStdinCommand, usize)> {
    for cmd in REF_STDIN_COMMANDS {
        let prefix = cmd.prefix.as_bytes();
        if !input.starts_with(prefix) {
            continue;
        }
        let sep = if cmd.args > 0 { b' ' } else { term };
        let after = input.get(prefix.len()).copied();
        // git compares input.buf[strlen(prefix)] against the expected
        // separator. For arg-less commands the buffer is terminator-stripped
        // here, so end-of-buffer also counts as the terminator.
        let matched = match after {
            Some(c) => c == sep,
            None => cmd.args == 0,
        };
        if matched {
            // Arguments begin after the prefix and its separator (if the
            // command takes one). git advances by `strlen(prefix) + !!args`.
            let start = prefix.len() + usize::from(cmd.args > 0 && after.is_some());
            return Some((cmd, start.min(input.len())));
        }
    }
    None
}

fn update_ref_stdin(context: UpdateRefStdinContext<'_>, deref: bool, nul: bool) -> Result<()> {
    if nul {
        return update_ref_stdin_z(&context, deref);
    }
    let mut deref = deref;
    let mut transaction = UpdateRefStdinTransaction::default();
    let stdin = io::stdin();
    let mut reader =
        crate::commands::stdin_stream::StdinRecordReader::new(stdin.lock(), b'\n');
    let mut stdout = io::stdout().lock();
    while let Some(mut line) = reader.read_record()? {
        crate::commands::stdin_stream::strip_trailing_cr(&mut line);
        let result = update_ref_stdin_line(&context, &mut deref, &mut transaction, &mut stdout, &line);
        if let Err(err) = result {
            let _ = transaction.restore(context.store);
            return Err(err);
        }
        stdout.flush()?;
    }
    transaction.finish_implicit(context.store)
}

fn update_ref_stdin_line(
    context: &UpdateRefStdinContext<'_>,
    deref: &mut bool,
    transaction: &mut UpdateRefStdinTransaction,
    stdout: &mut dyn Write,
    line: &[u8],
) -> Result<()> {
    use crate::commands::ref_command_stream::{classify_line, ArgCursor, Terminator};

    // git's first two guards: a bare terminator is `empty command in input`,
    // leading whitespace is `whitespace before command: <line>`.
    classify_line(line)?;

    let Some((cmd, arg_start)) = match_ref_stdin_command(line, b'\n') else {
        return update_ref_stdin_bad_command(&String::from_utf8_lossy(line));
    };
    let cursor = ArgCursor::new(&line[arg_start..], Terminator::Newline);
    dispatch_ref_stdin_command(context, deref, transaction, stdout, cmd, cursor)
}

fn update_ref_stdin_z(context: &UpdateRefStdinContext<'_>, deref: bool) -> Result<()> {
    use crate::commands::ref_command_stream::{ArgCursor, Terminator};

    let stdin = io::stdin();
    let mut reader = crate::commands::stdin_stream::StdinRecordReader::new(stdin.lock(), b'\0');
    let mut stdout = io::stdout().lock();
    let mut deref = deref;
    let mut transaction = UpdateRefStdinTransaction::default();
    while let Some(first) = reader.read_record()? {
        // git guards: an empty command record is `empty command in input`; a
        // record beginning with whitespace is `whitespace before command`.
        // (Under -z, `echo "" | git update-ref -z --stdin` yields a record of
        // "\n" which trips the whitespace guard, while `printf '\0'` yields a
        // truly empty record which trips the empty-command guard.)
        crate::commands::ref_command_stream::classify_line(&first)?;

        let Some((cmd, arg_start)) =
            match_ref_stdin_command(&first, b'\0')
        else {
            transaction.restore(context.store)?;
            return update_ref_stdin_bad_command(&String::from_utf8_lossy(&first));
        };

        // Stitch the command record with `cmd.args - 1` additional NUL records,
        // exactly as git pre-reads them in its main loop. The stitched buffer
        // uses `\0` between records and a trailing `\0` so the cursor sees the
        // same shape git's `input` strbuf has after the appends.
        let mut stitched = first[arg_start..].to_vec();
        let mut early_eof = false;
        for _ in 1..cmd.args {
            stitched.push(b'\0');
            match reader.read_record()? {
                Some(rec) => stitched.extend_from_slice(&rec),
                None => {
                    early_eof = true;
                    break;
                }
            }
        }
        if cmd.args > 0 && !early_eof {
            stitched.push(b'\0');
        }

        let cursor = ArgCursor::new(&stitched, Terminator::Nul);
        let result =
            dispatch_ref_stdin_command(context, &mut deref, &mut transaction, &mut stdout, cmd, cursor);
        if let Err(err) = result {
            transaction.restore(context.store)?;
            return Err(err);
        }
        stdout.flush()?;
    }
    transaction.finish_implicit(context.store)
}

/// Shared dispatch for both the `\n` and `-z` paths, mirroring git's
/// `parse_cmd_*` family. `cursor` is positioned at the first argument; the
/// command's `Terminator` carries whether we are in text or binary mode.
fn dispatch_ref_stdin_command(
    context: &UpdateRefStdinContext<'_>,
    deref: &mut bool,
    transaction: &mut UpdateRefStdinTransaction,
    stdout: &mut dyn Write,
    cmd: &RefStdinCommand,
    mut cursor: crate::commands::ref_command_stream::ArgCursor<'_>,
) -> Result<()> {
    use crate::commands::ref_command_stream::NextOid;

    let verb = cmd.prefix;
    if transaction.is_closed() && verb != "start" {
        return update_ref_stdin_closed_transaction();
    }
    if transaction.is_prepared() && !matches!(verb, "commit" | "abort") {
        return update_ref_stdin_prepared_transaction();
    }

    match verb {
        "option" => {
            // git: `parse_cmd_option` checks the entire remainder against
            // `no-deref<term>`; anything else dies with `option unknown: <rest>`
            // where <rest> is the raw remainder (no C-unquoting). In `\n` mode
            // git's `next` still carries the trailing newline, so the message
            // gains an extra blank line; in `-z` mode the NUL is not printed.
            let rest = cursor.remainder();
            if rest == "no-deref" {
                *deref = false;
                Ok(())
            } else {
                update_ref_stdin_unknown_option(&rest, cursor.terminator_byte())
            }
        }
        "update" => {
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            // <new-oid> allows empty (treated as zero, with a -z warning).
            let new = match cursor.parse_next_oid("update", &raw_name, true)? {
                NextOid::Missing => return update_ref_stdin_missing_new_oid("update", &raw_name),
                NextOid::Eof => {
                    return update_ref_stdin_eof("update", &raw_name, "<new-oid>");
                }
                NextOid::Zero => {
                    if cursor.terminator_byte() == b'\0' {
                        eprintln!(
                            "warning: update {raw_name}: missing <new-oid>, treating as zero"
                        );
                    }
                    None
                }
                NextOid::Value(v) => Some(v),
            };
            // <old-oid> does NOT allow empty: a present empty value is a zero
            // OID (verify the ref does not currently exist), distinct from an
            // absent value (no old-value check).
            let old = match cursor.parse_next_oid("update", &raw_name, false)? {
                NextOid::Missing => OldOid::Absent,
                NextOid::Zero => OldOid::Zero,
                NextOid::Eof => {
                    return update_ref_stdin_eof("update", &raw_name, "<old-oid>");
                }
                NextOid::Value(v) => OldOid::Value(v),
            };
            cursor.finish("update", &raw_name)?;

            let new_oid = match new {
                Some(v) => resolve_stdin_oid(
                    context.git_dir,
                    context.format,
                    context.store,
                    "update",
                    &raw_name,
                    "<new-oid>",
                    &v,
                )?,
                None => zero_oid(context.format)?,
            };
            let expected = match old {
                OldOid::Absent => None,
                OldOid::Zero => Some(zero_oid(context.format)?),
                OldOid::Value(v) => Some(resolve_stdin_oid(
                    context.git_dir,
                    context.format,
                    context.store,
                    "update",
                    &raw_name,
                    "<old-oid>",
                    &v,
                )?),
            };
            let name = update_ref_effective_name(context.store, &raw_name, *deref)?;
            if context.batch_updates {
                return update_ref_stdin_write_batch(
                    context,
                    UpdateRefStdinWriteRequest {
                        name,
                        new_oid,
                        expected_oid: expected.as_ref(),
                    },
                    stdout,
                );
            }
            if transaction.capture(context.store, &name)? {
                return Ok(());
            }
            update_ref_stdin_write(
                context,
                UpdateRefStdinWriteRequest {
                    name,
                    new_oid,
                    expected_oid: expected.as_ref(),
                },
            )
        }
        "create" => {
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            // create's <new-oid> does not allow empty: `-z` empty / absent is
            // `missing <new-oid>`, while a `\n` empty value decodes to zero and
            // falls through to the `zero <new-oid>` guard below.
            let new = match cursor.parse_next_oid("create", &raw_name, false)? {
                NextOid::Missing => {
                    return update_ref_stdin_missing_new_oid("create", &raw_name);
                }
                NextOid::Eof => {
                    return update_ref_stdin_eof("create", &raw_name, "<new-oid>");
                }
                NextOid::Zero => None,
                NextOid::Value(v) => Some(v),
            };
            cursor.finish("create", &raw_name)?;

            let new_oid = match new {
                Some(v) => resolve_stdin_oid(
                    context.git_dir,
                    context.format,
                    context.store,
                    "create",
                    &raw_name,
                    "<new-oid>",
                    &v,
                )?,
                None => zero_oid(context.format)?,
            };
            if new_oid == zero_oid(context.format)? {
                return update_ref_stdin_create_zero(&raw_name);
            }
            let name = update_ref_effective_name(context.store, &raw_name, *deref)?;
            let zero = zero_oid(context.format)?;
            if context.batch_updates {
                return update_ref_stdin_write_batch(
                    context,
                    UpdateRefStdinWriteRequest {
                        name,
                        new_oid,
                        expected_oid: Some(&zero),
                    },
                    stdout,
                );
            }
            if transaction.capture(context.store, &name)? {
                return Ok(());
            }
            update_ref_stdin_write(
                context,
                UpdateRefStdinWriteRequest {
                    name,
                    new_oid,
                    expected_oid: Some(&zero),
                },
            )
        }
        "delete" => {
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            let old = match cursor.parse_next_oid("delete", &raw_name, false)? {
                NextOid::Missing => OldOid::Absent,
                NextOid::Zero => OldOid::Zero,
                NextOid::Eof => {
                    return update_ref_stdin_eof("delete", &raw_name, "<old-oid>");
                }
                NextOid::Value(v) => OldOid::Value(v),
            };
            cursor.finish("delete", &raw_name)?;

            let expected = match old {
                OldOid::Absent => None,
                // git: a `\n`-empty <old-oid> for delete is a zero, which is an
                // error.
                OldOid::Zero => return update_ref_stdin_delete_zero(&raw_name),
                OldOid::Value(v) => {
                    let oid = resolve_stdin_oid(
                        context.git_dir,
                        context.format,
                        context.store,
                        "delete",
                        &raw_name,
                        "<old-oid>",
                        &v,
                    )?;
                    // git: a resolved zero <old-oid> is also rejected.
                    if oid == zero_oid(context.format)? {
                        return update_ref_stdin_delete_zero(&raw_name);
                    }
                    Some(oid)
                }
            };
            let name = update_ref_effective_name(context.store, &raw_name, *deref)?;
            if context.batch_updates {
                return update_ref_delete_stdin_batch(
                    context.store,
                    context.format,
                    &name,
                    expected.as_ref(),
                    stdout,
                );
            }
            if transaction.capture(context.store, &name)? {
                return Ok(());
            }
            update_ref_delete_stdin(context.store, context.format, &name, expected.as_ref())
        }
        "verify" => {
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            // git's parse_cmd_verify clears <old-oid> to zero when absent, so
            // both an absent and a present-empty value verify against zero.
            let old = match cursor.parse_next_oid("verify", &raw_name, false)? {
                NextOid::Missing | NextOid::Zero => None,
                NextOid::Eof => {
                    return update_ref_stdin_eof("verify", &raw_name, "<old-oid>");
                }
                NextOid::Value(v) => Some(v),
            };
            cursor.finish("verify", &raw_name)?;

            let expected = match old {
                Some(v) => resolve_stdin_oid(
                    context.git_dir,
                    context.format,
                    context.store,
                    "verify",
                    &raw_name,
                    "<old-oid>",
                    &v,
                )?,
                None => zero_oid(context.format)?,
            };
            let name = update_ref_effective_name(context.store, &raw_name, *deref)?;
            let current = context.store.read_ref(&name)?;
            if context.batch_updates {
                return verify_update_ref_stdin_batch(
                    context.format,
                    &name,
                    current.as_ref(),
                    &expected,
                    stdout,
                );
            }
            check_update_ref_stdin_expected(context.format, &name, current.as_ref(), &expected)
        }
        "symref-create" => {
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            let Some(target) = cursor.parse_next_refname()? else {
                return update_ref_stdin_symref_update_missing_new_target_for("symref-create", &raw_name);
            };
            cursor.finish("symref-create", &raw_name)?;

            let name = update_ref_effective_name(context.store, &raw_name, *deref)?;
            if transaction.capture(context.store, &name)? {
                return Ok(());
            }
            update_ref_stdin_symref_create(context.store, &name, &target)
        }
        "symref-update" => {
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            let Some(target) = cursor.parse_next_refname()? else {
                return update_ref_stdin_symref_update_missing_new_target(&raw_name);
            };
            // Optional `<old-arg> <old-value>` pair: `ref <name>` or `oid <oid>`.
            let expected = match cursor.parse_next_arg()? {
                None => None,
                Some(old_arg) => {
                    let Some(old_value) = cursor.parse_next_arg()? else {
                        return update_ref_stdin_symref_update_missing_old_value(&raw_name);
                    };
                    match old_arg.as_str() {
                        "ref" => Some(UpdateRefStdinSymrefExpected::Target(old_value)),
                        "oid" => Some(UpdateRefStdinSymrefExpected::Oid(
                            match parse_update_ref_oidish(
                                context.git_dir,
                                context.format,
                                context.store,
                                &old_value,
                            ) {
                                Some(oid) => oid,
                                None => {
                                    eprintln!(
                                        "fatal: symref-update {raw_name}: invalid oid: {old_value}"
                                    );
                                    return Err(GitError::Exit(128));
                                }
                            },
                        )),
                        other => {
                            return update_ref_stdin_symref_update_invalid_old_kind(
                                &raw_name, other,
                            );
                        }
                    }
                }
            };
            cursor.finish("symref-update", &raw_name)?;

            let name = update_ref_effective_name(context.store, &raw_name, *deref)?;
            if transaction.capture(context.store, &name)? {
                return Ok(());
            }
            update_ref_stdin_symref_update(context.store, context.format, &name, &target, expected)
        }
        "symref-verify" => {
            if *deref {
                return update_ref_stdin_symref_verify_deref_mode();
            }
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            let expected = cursor.parse_next_refname()?;
            cursor.finish("symref-verify", &raw_name)?;
            update_ref_stdin_symref_verify(context.store, &raw_name, expected.as_deref())
        }
        "symref-delete" => {
            if *deref {
                return update_ref_stdin_symref_delete_deref_mode();
            }
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            let expected = cursor.parse_next_refname()?;
            cursor.finish("symref-delete", &raw_name)?;
            if transaction.capture(context.store, &raw_name)? {
                return Ok(());
            }
            update_ref_stdin_symref_delete(context.store, &raw_name, expected.as_deref())
        }
        "start" => {
            cursor.finish("start", "")?;
            transaction.start(stdout)
        }
        "prepare" => {
            cursor.finish("prepare", "")?;
            transaction.prepare(context.git_dir, context.store, stdout)
        }
        "commit" => {
            cursor.finish("commit", "")?;
            transaction.commit(context.store, stdout)
        }
        "abort" => {
            cursor.finish("abort", "")?;
            transaction.abort(context.store, stdout)
        }
        _ => update_ref_stdin_bad_command(verb),
    }
}

struct UpdateRefStdinTransaction {
    active: bool,
    explicit: bool,
    prepared: bool,
    closed: bool,
    originals: BTreeMap<String, Option<RefTarget>>,
    duplicate: Option<String>,
    locks: Vec<PathBuf>,
}

impl Default for UpdateRefStdinTransaction {
    fn default() -> Self {
        Self {
            active: true,
            explicit: false,
            prepared: false,
            closed: false,
            originals: BTreeMap::new(),
            duplicate: None,
            locks: Vec::new(),
        }
    }
}

impl UpdateRefStdinTransaction {
    fn capture(&mut self, store: &FileRefStore, name: &str) -> Result<bool> {
        if self.active {
            if self.originals.contains_key(name) {
                if self.duplicate.is_none() {
                    self.duplicate = Some(name.to_string());
                }
                return Ok(true);
            } else {
                self.originals
                    .insert(name.to_string(), store.read_ref(name)?);
            }
        }
        Ok(false)
    }

    fn is_prepared(&self) -> bool {
        self.prepared
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    fn start(&mut self, stdout: &mut dyn Write) -> Result<()> {
        if self.explicit {
            return update_ref_stdin_restart_transaction();
        }
        self.active = true;
        self.explicit = true;
        self.prepared = false;
        self.closed = false;
        writeln!(stdout, "start: ok")?;
        Ok(())
    }

    fn prepare(
        &mut self,
        git_dir: &Path,
        store: &FileRefStore,
        stdout: &mut dyn Write,
    ) -> Result<()> {
        if let Some(name) = self.duplicate.clone() {
            self.restore(store)?;
            eprintln!("fatal: prepare: multiple updates for ref '{name}' not allowed");
            return Err(GitError::Exit(128));
        }
        self.acquire_locks(git_dir)?;
        self.prepared = true;
        writeln!(stdout, "prepare: ok")?;
        Ok(())
    }

    fn commit(&mut self, store: &FileRefStore, stdout: &mut dyn Write) -> Result<()> {
        if let Some(name) = self.duplicate.clone() {
            self.restore(store)?;
            eprintln!("fatal: commit: multiple updates for ref '{name}' not allowed");
            return Err(GitError::Exit(128));
        }
        self.active = false;
        self.explicit = false;
        self.prepared = false;
        self.closed = true;
        self.release_locks();
        self.originals.clear();
        self.duplicate = None;
        writeln!(stdout, "commit: ok")?;
        Ok(())
    }

    fn finish_implicit(&mut self, store: &FileRefStore) -> Result<()> {
        if self.explicit || self.prepared {
            return self.restore(store);
        }
        if let Some(name) = self.duplicate.clone() {
            self.restore(store)?;
            eprintln!("fatal: multiple updates for ref '{name}' not allowed");
            return Err(GitError::Exit(128));
        }
        self.active = false;
        self.prepared = false;
        self.originals.clear();
        self.closed = false;
        Ok(())
    }

    fn abort(&mut self, store: &FileRefStore, stdout: &mut dyn Write) -> Result<()> {
        self.restore(store)?;
        writeln!(stdout, "abort: ok")?;
        Ok(())
    }

    fn restore(&mut self, store: &FileRefStore) -> Result<()> {
        self.release_locks();
        if self.active {
            for (name, original) in mem::take(&mut self.originals) {
                update_ref_stdin_restore_ref(store, &name, original)?;
            }
            self.active = false;
        }
        self.explicit = false;
        self.prepared = false;
        self.closed = true;
        self.duplicate = None;
        Ok(())
    }

    fn acquire_locks(&mut self, git_dir: &Path) -> Result<()> {
        if !self.locks.is_empty() {
            return Ok(());
        }
        let names = self.originals.keys().cloned().collect::<Vec<_>>();
        for name in names {
            let path = loose_ref_path_for_ref(git_dir, &name)?;
            let parent = path
                .parent()
                .ok_or_else(|| GitError::InvalidPath("ref path has no parent".into()))?;
            fs::create_dir_all(parent)?;
            let lock_path = lock_path_for_loose_ref_path(&path)?;
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => self.locks.push(lock_path),
                Err(err) => {
                    self.release_locks();
                    eprintln!("fatal: prepare: cannot lock ref '{name}': {err}");
                    return Err(GitError::Exit(128));
                }
            }
        }
        Ok(())
    }

    fn release_locks(&mut self) {
        for lock in mem::take(&mut self.locks) {
            let _ = fs::remove_file(lock);
        }
    }
}

fn update_ref_stdin_prepared_transaction() -> Result<()> {
    eprintln!("fatal: prepared transactions can only be closed");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_restart_transaction() -> Result<()> {
    eprintln!("fatal: cannot restart ongoing transaction");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_closed_transaction() -> Result<()> {
    eprintln!("fatal: transaction is closed");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_restore_ref(
    store: &FileRefStore,
    name: &str,
    original: Option<RefTarget>,
) -> Result<()> {
    update_ref_stdin_remove_ref(store, name)?;
    if let Some(target) = original {
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: name.to_string(),
            expected: None,
            new: target,
            reflog: None,
        });
        tx.commit()?;
    }
    Ok(())
}

fn update_ref_stdin_remove_ref(store: &FileRefStore, name: &str) -> Result<()> {
    match store.read_ref(name)? {
        Some(RefTarget::Symbolic(_)) => {
            let _ = store.delete_symbolic_ref(name)?;
        }
        Some(RefTarget::Direct(_)) => {
            let _ = store.delete_ref(name)?;
        }
        None => {}
    }
    Ok(())
}

fn update_ref_stdin_bad_command(command: &str) -> Result<()> {
    eprintln!("fatal: unknown command: {command}");
    Err(GitError::Exit(128))
}

/// git's `<cmd>: missing <ref>` (e.g. `create: missing <ref>`).
fn update_ref_stdin_missing_ref(command: &str) -> Result<()> {
    eprintln!("fatal: {command}: missing <ref>");
    Err(GitError::Exit(128))
}

/// git's `<cmd> <ref>: missing <new-oid>` for create/update.
fn update_ref_stdin_missing_new_oid(command: &str, refname: &str) -> Result<()> {
    eprintln!("fatal: {command} {refname}: missing <new-oid>");
    Err(GitError::Exit(128))
}

/// git's `<cmd> <ref>: unexpected end of input when reading <field>` (only the
/// `-z` path can hit this; the `\n` path treats a short line as `missing`).
fn update_ref_stdin_eof(command: &str, refname: &str, field: &str) -> Result<()> {
    eprintln!("fatal: {command} {refname}: unexpected end of input when reading {field}");
    Err(GitError::Exit(128))
}

/// git's `<cmd> <ref>: missing <new-target>` for symref-create.
fn update_ref_stdin_symref_update_missing_new_target_for(
    command: &str,
    refname: &str,
) -> Result<()> {
    eprintln!("fatal: {command} {refname}: missing <new-target>");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_unknown_option(option: &str, terminator: u8) -> Result<()> {
    // git's `die("option unknown: %s", next)` prints `next` (the raw tail) and
    // then a newline. In `\n` mode `next` still includes the line's trailing
    // newline, producing a blank line after the message; in `-z` mode the NUL
    // is not printed by `%s`, so there is only the single die newline.
    if terminator == b'\n' {
        eprintln!("fatal: option unknown: {option}\n");
    } else {
        eprintln!("fatal: option unknown: {option}");
    }
    Err(GitError::Exit(128))
}

fn update_ref_stdin_create_zero(name: &str) -> Result<()> {
    eprintln!("fatal: create {name}: zero <new-oid>");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_delete_zero(name: &str) -> Result<()> {
    eprintln!("fatal: delete {name}: zero <old-oid>");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_symref_create(store: &FileRefStore, name: &str, target: &str) -> Result<()> {
    validate_ref_name(name)?;
    if let Some(current) = store.read_ref(name)? {
        return match current {
            RefTarget::Symbolic(_) => update_ref_stdin_symref_exists(name, true),
            RefTarget::Direct(_) => update_ref_stdin_symref_exists(name, false),
        };
    }
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: name.to_string(),
        expected: None,
        new: RefTarget::Symbolic(target.to_string()),
        reflog: None,
    });
    tx.commit()
}

enum UpdateRefStdinSymrefExpected {
    Target(String),
    Oid(ObjectId),
}

fn update_ref_stdin_symref_update(
    store: &FileRefStore,
    format: ObjectFormat,
    name: &str,
    target: &str,
    expected: Option<UpdateRefStdinSymrefExpected>,
) -> Result<()> {
    validate_ref_name(name)?;
    match expected {
        Some(UpdateRefStdinSymrefExpected::Target(expected)) => {
            update_ref_stdin_symref_verify(store, name, Some(&expected))?;
        }
        Some(UpdateRefStdinSymrefExpected::Oid(expected)) => {
            let current = store.read_ref(name)?;
            update_ref_stdin_symref_verify_oid(format, name, current.as_ref(), &expected)?;
        }
        None => {}
    }
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: name.to_string(),
        expected: None,
        new: RefTarget::Symbolic(target.to_string()),
        reflog: None,
    });
    tx.commit()
}

fn update_ref_stdin_symref_verify(
    store: &FileRefStore,
    name: &str,
    expected: Option<&str>,
) -> Result<()> {
    validate_ref_name(name)?;
    match (store.read_ref(name)?, expected) {
        (None, None) => Ok(()),
        (None, Some(_)) | (Some(RefTarget::Direct(_)), Some(_)) => {
            update_ref_stdin_symref_unresolved(name)
        }
        (Some(RefTarget::Direct(_)), None) => update_ref_stdin_symref_exists(name, false),
        (Some(RefTarget::Symbolic(_)), None) => update_ref_stdin_symref_exists(name, true),
        (Some(RefTarget::Symbolic(actual)), Some(expected)) if actual == expected => Ok(()),
        (Some(RefTarget::Symbolic(actual)), Some(expected)) => {
            eprintln!(
                "fatal: verifying symref target: '{name}': is at {actual} but expected {expected}"
            );
            Err(GitError::Exit(128))
        }
    }
}

fn update_ref_stdin_symref_verify_oid(
    format: ObjectFormat,
    name: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
) -> Result<()> {
    let zero = zero_oid(format)?;
    if matches!(current, Some(RefTarget::Symbolic(_))) && expected != &zero {
        eprintln!("fatal: cannot lock ref '{name}': reference is missing but expected {expected}");
        return Err(GitError::Exit(128));
    }
    check_update_ref_stdin_expected(format, name, current, expected)
}

fn update_ref_stdin_symref_delete(
    store: &FileRefStore,
    name: &str,
    expected: Option<&str>,
) -> Result<()> {
    update_ref_stdin_symref_verify(store, name, expected)?;
    if store.read_ref(name)?.is_some() {
        let _ = store.delete_symbolic_ref(name)?;
    }
    Ok(())
}

fn update_ref_stdin_symref_verify_deref_mode() -> Result<()> {
    eprintln!("fatal: symref-verify: cannot operate with deref mode");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_symref_delete_deref_mode() -> Result<()> {
    eprintln!("fatal: symref-delete: cannot operate with deref mode");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_symref_exists(name: &str, symbolic: bool) -> Result<()> {
    let reason = if symbolic {
        "dangling symref already exists"
    } else {
        "reference already exists"
    };
    eprintln!("fatal: cannot lock ref '{name}': {reason}");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_symref_unresolved(name: &str) -> Result<()> {
    eprintln!("fatal: cannot lock ref '{name}': unable to resolve reference '{name}'");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_symref_update_missing_new_target(name: &str) -> Result<()> {
    eprintln!("fatal: symref-update {name}: missing <new-target>");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_symref_update_missing_old_value(name: &str) -> Result<()> {
    eprintln!("fatal: symref-update {name}: expected old value");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_symref_update_invalid_old_kind(name: &str, kind: &str) -> Result<()> {
    eprintln!("fatal: symref-update {name}: invalid arg '{kind}' for old value");
    Err(GitError::Exit(128))
}

#[derive(Debug)]
struct UpdateRefStdinRejection {
    name: String,
    new_value: String,
    old_value: String,
    stdout_reason: &'static str,
    stderr_reason: String,
}

fn print_update_ref_stdin_rejection(
    rejection: UpdateRefStdinRejection,
    stdout: &mut dyn Write,
) -> Result<()> {
    writeln!(
        stdout,
        "rejected {} {} {} {}",
        rejection.name, rejection.new_value, rejection.old_value, rejection.stdout_reason
    )?;
    eprintln!(
        "error: cannot lock ref '{}': {}",
        rejection.name, rejection.stderr_reason
    );
    Ok(())
}

fn update_ref_stdin_expected_rejection(
    format: ObjectFormat,
    name: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
    new_value: String,
) -> Result<Option<UpdateRefStdinRejection>> {
    let zero = zero_oid(format)?;
    if expected == &zero {
        if current.is_some() {
            return Ok(Some(UpdateRefStdinRejection {
                name: name.to_string(),
                new_value,
                old_value: zero.to_string(),
                stdout_reason: "reference already exists",
                stderr_reason: "reference already exists".to_string(),
            }));
        }
        return Ok(None);
    }

    match current {
        Some(RefTarget::Direct(actual)) if actual == expected => Ok(None),
        Some(RefTarget::Direct(actual)) => Ok(Some(UpdateRefStdinRejection {
            name: name.to_string(),
            new_value,
            old_value: expected.to_string(),
            stdout_reason: "incorrect old value provided",
            stderr_reason: format!("is at {actual} but expected {expected}"),
        })),
        Some(RefTarget::Symbolic(_)) | None => Ok(Some(UpdateRefStdinRejection {
            name: name.to_string(),
            new_value,
            old_value: expected.to_string(),
            stdout_reason: "reference does not exist",
            stderr_reason: format!("unable to resolve reference '{name}'"),
        })),
    }
}

fn check_update_ref_new_value(
    git_dir: &Path,
    format: ObjectFormat,
    name: &str,
    new_oid: &ObjectId,
) -> std::result::Result<(), String> {
    if new_oid == &zero_oid(format).map_err(|err| err.to_string())? {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(new_oid).map_err(|err| match err {
        GitError::NotFound(_) => {
            format!("trying to write ref '{name}' with nonexistent object {new_oid}")
        }
        err => err.to_string(),
    })?;
    if update_ref_requires_commit(name) && object.object_type != ObjectType::Commit {
        return Err(format!(
            "trying to write non-commit object {new_oid} to branch '{name}'"
        ));
    }
    Ok(())
}

fn update_ref_requires_commit(name: &str) -> bool {
    name == "HEAD" || name.starts_with("refs/heads/")
}

fn update_ref_stdin_write_batch(
    context: &UpdateRefStdinContext<'_>,
    request: UpdateRefStdinWriteRequest<'_>,
    stdout: &mut dyn Write,
) -> Result<()> {
    let current = context.store.read_ref(&request.name)?;
    if let Some(expected_oid) = request.expected_oid
        && let Some(rejection) = update_ref_stdin_expected_rejection(
            context.format,
            &request.name,
            current.as_ref(),
            expected_oid,
            request.new_oid.to_string(),
        )?
    {
        print_update_ref_stdin_rejection(rejection, stdout)?;
        return Ok(());
    }
    if request.new_oid == zero_oid(context.format)? {
        return update_ref_delete_stdin(context.store, context.format, &request.name, None);
    }
    if let Err(reason) = check_update_ref_new_value(
        context.git_dir,
        context.format,
        &request.name,
        &request.new_oid,
    ) {
        writeln!(
            stdout,
            "rejected {} {} {} invalid new value provided",
            request.name,
            request.new_oid,
            request
                .expected_oid
                .map(ObjectId::to_string)
                .unwrap_or_else(|| "(null)".to_string())
        )?;
        eprintln!("error: cannot update ref '{}': {reason}", request.name);
        return Ok(());
    }
    update_ref_stdin_write(
        context,
        UpdateRefStdinWriteRequest {
            expected_oid: None,
            ..request
        },
    )
}

fn update_ref_delete_stdin_batch(
    store: &FileRefStore,
    format: ObjectFormat,
    name: &str,
    expected: Option<&ObjectId>,
    stdout: &mut dyn Write,
) -> Result<()> {
    let current = store.read_ref(name)?;
    if let Some(expected) = expected
        && let Some(rejection) = update_ref_stdin_expected_rejection(
            format,
            name,
            current.as_ref(),
            expected,
            zero_oid(format)?.to_string(),
        )?
    {
        print_update_ref_stdin_rejection(rejection, stdout)?;
        return Ok(());
    }
    update_ref_delete_stdin(store, format, name, None)
}

fn verify_update_ref_stdin_batch(
    format: ObjectFormat,
    name: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
    stdout: &mut dyn Write,
) -> Result<()> {
    if let Some(rejection) =
        update_ref_stdin_expected_rejection(format, name, current, expected, "(null)".to_string())?
    {
        print_update_ref_stdin_rejection(rejection, stdout)?;
    }
    Ok(())
}

fn update_ref_stdin_write(
    context: &UpdateRefStdinContext<'_>,
    request: UpdateRefStdinWriteRequest<'_>,
) -> Result<()> {
    let current = context.store.read_ref(&request.name)?;
    if let Some(expected_oid) = request.expected_oid {
        check_update_ref_stdin_expected(
            context.format,
            &request.name,
            current.as_ref(),
            expected_oid,
        )?;
    }
    if request.new_oid == zero_oid(context.format)? {
        return update_ref_delete_stdin(context.store, context.format, &request.name, None);
    }
    check_update_ref_new_value(
        context.git_dir,
        context.format,
        &request.name,
        &request.new_oid,
    )
    .map_err(|reason| {
        eprintln!("fatal: cannot update ref '{}': {reason}", request.name);
        GitError::Exit(128)
    })?;
    let old_oid = match current {
        Some(RefTarget::Direct(oid)) => oid,
        _ => zero_oid(context.format)?,
    };
    let reflog =
        update_ref_should_write_reflog(context.git_dir, &request.name, context.create_reflog)?
            .then(|| ReflogEntry {
                old_oid,
                new_oid: request.new_oid,
                committer: default_committer(),
                message: context.message.clone(),
            });
    let mut tx = context.store.transaction();
    tx.update(RefUpdate {
        name: request.name,
        expected: None,
        new: RefTarget::Direct(request.new_oid),
        reflog,
    });
    tx.commit()
}

fn update_ref_effective_name(store: &FileRefStore, name: &str, deref: bool) -> Result<String> {
    if !deref {
        return Ok(name.to_string());
    }
    let mut current = name.to_string();
    for _ in 0..16 {
        match store.read_ref(&current)? {
            Some(RefTarget::Symbolic(target)) => current = target,
            _ => return Ok(current),
        }
    }
    Ok(current)
}

fn update_ref_delete(
    store: &FileRefStore,
    format: ObjectFormat,
    name: &str,
    expected: Option<&ObjectId>,
) -> Result<()> {
    let current = store.read_ref(name)?;
    if let Some(expected) = expected {
        let zero = zero_oid(format)?;
        if expected != &zero {
            match current.as_ref() {
                Some(RefTarget::Direct(actual)) if actual == expected => {}
                Some(RefTarget::Direct(actual)) => {
                    return update_ref_delete_lock_failure(
                        name,
                        &format!("is at {actual} but expected {expected}"),
                    );
                }
                Some(RefTarget::Symbolic(_)) | None => {
                    return update_ref_delete_lock_failure(
                        name,
                        &format!("unable to resolve reference '{name}'"),
                    );
                }
            }
        }
    }
    if current.is_some() {
        match current {
            Some(RefTarget::Symbolic(_)) => {
                store.delete_symbolic_ref(name)?;
            }
            _ => {
                store.delete_ref(name)?;
            }
        }
    }
    Ok(())
}

fn update_ref_delete_stdin(
    store: &FileRefStore,
    format: ObjectFormat,
    name: &str,
    expected: Option<&ObjectId>,
) -> Result<()> {
    let current = store.read_ref(name)?;
    if let Some(expected) = expected {
        let zero = zero_oid(format)?;
        if expected != &zero {
            match current.as_ref() {
                Some(RefTarget::Direct(actual)) if actual == expected => {}
                Some(RefTarget::Direct(actual)) => {
                    return update_ref_stdin_lock_failure(
                        name,
                        &format!("is at {actual} but expected {expected}"),
                    );
                }
                Some(RefTarget::Symbolic(_)) | None => {
                    return update_ref_stdin_lock_failure(
                        name,
                        &format!("unable to resolve reference '{name}'"),
                    );
                }
            }
        }
    }
    if current.is_some() {
        match current {
            Some(RefTarget::Symbolic(_)) => {
                store.delete_symbolic_ref(name)?;
            }
            _ => {
                store.delete_ref(name)?;
            }
        }
    }
    Ok(())
}

fn update_ref_should_write_reflog(git_dir: &Path, name: &str, create_reflog: bool) -> Result<bool> {
    if reflog_path_for_ref(git_dir, name)?.exists() || create_reflog {
        return Ok(true);
    }
    if let Some(value) = global_config_value("core.logAllRefUpdates")? {
        return Ok(update_ref_log_all_ref_updates_matches(name, &value));
    }
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let Ok(config) = GitConfig::read(common_git_dir.join("config")) else {
        return Ok(false);
    };
    Ok(config
        .get("core", None, "logAllRefUpdates")
        .is_some_and(|value| update_ref_log_all_ref_updates_matches(name, value)))
}

fn update_ref_log_all_ref_updates_matches(name: &str, value: &str) -> bool {
    if value.eq_ignore_ascii_case("always") {
        return true;
    }
    if !sley_config::parse_config_bool(value).unwrap_or(false) {
        return false;
    }
    name == "HEAD"
        || name.starts_with("refs/heads/")
        || name.starts_with("refs/remotes/")
        || name.starts_with("refs/notes/")
}

fn parse_update_ref_new_oid(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    value: &str,
) -> Result<ObjectId> {
    if let Some(oid) = parse_update_ref_oidish(git_dir, format, store, value) {
        return Ok(oid);
    }
    eprintln!(
        "fatal: invalid object id: expected {} hex digits for {}, got {}",
        format.hex_len(),
        format.name(),
        value.len()
    );
    Err(GitError::Exit(128))
}

fn parse_update_ref_oidish(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    value: &str,
) -> Option<ObjectId> {
    if let Ok(oid) = ObjectId::from_hex(format, value) {
        return Some(oid);
    }
    if let Ok(Some(oid)) = resolve_ref_peeled(store, value) {
        return Some(oid);
    }
    // git's update-ref resolves <newvalue> as a revision, so revision syntax like
    // `HEAD~1` or `<branch>^0` works (e.g. t1500's bisect setup, driven by
    // test_commit_bulk). Fall through to the full revision parser, storing
    // whatever object it resolves to (git does not peel here). `resolve_revision`
    // also covers the plain-ref case (a bare `refs/...` or `HEAD`), so a
    // validation error from `resolve_ref_peeled` above is not fatal on its own.
    if let Ok(oid) = sley_rev::resolve_revision(git_dir, format, value) {
        return Some(oid);
    }
    None
}

fn parse_update_ref_expected(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    value: &str,
) -> Result<ObjectId> {
    parse_update_ref_oidish(git_dir, format, store, value).ok_or_else(|| {
        GitError::InvalidObjectId(format!(
            "expected {} hex digits for {}, got {}",
            format.hex_len(),
            format.name(),
            value.len()
        ))
    })
}

/// Resolve a `<new-oid>`/`<old-oid>` argument from `update-ref --stdin`,
/// emitting git's command-stream-specific die-message on failure:
/// `<cmd> <ref>: invalid <new-oid>: <value>` (or `<old-oid>`). git's
/// `parse_next_oid` produces these, distinct from the generic resolver's
/// message.
fn resolve_stdin_oid(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    command: &str,
    refname: &str,
    field: &str,
    value: &str,
) -> Result<ObjectId> {
    match parse_update_ref_oidish(git_dir, format, store, value) {
        Some(oid) => Ok(oid),
        None => {
            eprintln!("fatal: {command} {refname}: invalid {field}: {value}");
            Err(GitError::Exit(128))
        }
    }
}

fn check_update_ref_expected(
    format: ObjectFormat,
    name: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
) -> Result<()> {
    let zero = zero_oid(format)?;
    if expected == &zero {
        if current.is_some() {
            return update_ref_lock_failure(name, "reference already exists");
        }
        return Ok(());
    }

    match current {
        Some(RefTarget::Direct(actual)) if actual == expected => Ok(()),
        Some(RefTarget::Direct(actual)) => {
            update_ref_lock_failure(name, &format!("is at {actual} but expected {expected}"))
        }
        Some(RefTarget::Symbolic(_)) | None => {
            update_ref_lock_failure(name, &format!("unable to resolve reference '{name}'"))
        }
    }
}

fn check_update_ref_stdin_expected(
    format: ObjectFormat,
    name: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
) -> Result<()> {
    let zero = zero_oid(format)?;
    if expected == &zero {
        if current.is_some() {
            return update_ref_stdin_lock_failure(name, "reference already exists");
        }
        return Ok(());
    }

    match current {
        Some(RefTarget::Direct(actual)) if actual == expected => Ok(()),
        Some(RefTarget::Direct(actual)) => {
            update_ref_stdin_lock_failure(name, &format!("is at {actual} but expected {expected}"))
        }
        Some(RefTarget::Symbolic(_)) | None => {
            update_ref_stdin_lock_failure(name, &format!("unable to resolve reference '{name}'"))
        }
    }
}

fn update_ref_lock_failure(name: &str, reason: &str) -> Result<()> {
    eprintln!("fatal: update_ref failed for ref '{name}': cannot lock ref '{name}': {reason}");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_lock_failure(name: &str, reason: &str) -> Result<()> {
    eprintln!("fatal: cannot lock ref '{name}': {reason}");
    Err(GitError::Exit(128))
}

fn update_ref_delete_lock_failure(name: &str, reason: &str) -> Result<()> {
    eprintln!("error: cannot lock ref '{name}': {reason}");
    Err(GitError::Exit(1))
}

pub(crate) fn cmd_show_ref(args: &[String]) -> Result<()> {
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let refs = store.list_refs()?;
    let mut include_head = false;
    let mut heads = false;
    let mut tags = false;
    let mut verify = false;
    let mut exists = false;
    let mut quiet = false;
    let mut hash_only = false;
    let mut dereference = false;
    let mut exclude_existing = None;
    let mut abbrev = None;
    let mut filters = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            filters.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--head" => include_head = true,
            "--no-head" => include_head = false,
            "--heads" | "--branches" => heads = true,
            "--no-branches" => heads = false,
            "--tags" => tags = true,
            "--no-tags" => tags = false,
            "--verify" => verify = true,
            "--no-verify" => verify = false,
            "--exists" => exists = true,
            "--no-exists" => exists = false,
            "--quiet" | "-q" => quiet = true,
            "--no-quiet" => quiet = false,
            "--hash" | "--no-hash" | "-s" => hash_only = true,
            "--dereference" | "-d" => dereference = true,
            "--no-dereference" => dereference = false,
            "--exclude-existing" => exclude_existing = Some(String::new()),
            "--abbrev" => abbrev = Some(7),
            "--no-abbrev" => abbrev = None,
            value if value.starts_with('-') => {
                if parse_show_ref_short_options(
                    value,
                    &mut quiet,
                    &mut hash_only,
                    &mut dereference,
                    &mut abbrev,
                )? {
                    continue;
                }
                if let Some(value) = value.strip_prefix("-s")
                    && !value.is_empty()
                {
                    hash_only = true;
                    abbrev = Some(parse_abbrev(value)?);
                    continue;
                }
                if let Some(value) = value.strip_prefix("--exclude-existing=") {
                    exclude_existing = Some(value.to_string());
                    continue;
                }
                if let Some(value) = value.strip_prefix("--hash=") {
                    hash_only = true;
                    abbrev = Some(parse_abbrev(value)?);
                    continue;
                }
                if let Some(value) = value.strip_prefix("--abbrev=") {
                    abbrev = Some(parse_abbrev(value)?);
                    continue;
                }
                return Err(GitError::Command(format!(
                    "unsupported show-ref option {value}"
                )));
            }
            value => filters.push(value),
        }
    }
    if let Some(pattern) = exclude_existing {
        return cmd_show_ref_exclude_existing(
            &refs,
            (!pattern.is_empty()).then_some(pattern.as_str()),
        );
    }
    if exists {
        if filters.len() != 1 {
            return show_ref_exists_requires_reference(filters.len());
        }
        if show_ref_exists(&store, &refs, filters[0])? {
            return Ok(());
        }
        eprintln!("error: reference does not exist");
        return Err(GitError::Exit(2));
    }
    if verify {
        if filters.is_empty() {
            return Err(GitError::Command(
                "show-ref --verify requires <ref>...".into(),
            ));
        }
        for filter in filters {
            if filter == "HEAD" {
                let oid = resolve_revision(&git_dir, format, "HEAD")?;
                if !quiet {
                    print_show_ref(&oid, filter, hash_only, abbrev);
                    print_show_ref_deref(&db, format, &oid, filter, dereference, abbrev)?;
                }
                continue;
            }
            match store.read_ref(filter)? {
                Some(target) => {
                    let reference = sley_refs::Ref {
                        name: filter.to_string(),
                        target,
                    };
                    let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? else {
                        if quiet {
                            return Err(GitError::Exit(1));
                        }
                        eprintln!("fatal: '{filter}' - not a valid ref");
                        return Err(GitError::Exit(128));
                    };
                    if !quiet {
                        print_show_ref(&oid, filter, hash_only, abbrev);
                        print_show_ref_deref(&db, format, &oid, filter, dereference, abbrev)?;
                    }
                }
                None if quiet => return Err(GitError::Exit(1)),
                _ => {
                    eprintln!("fatal: '{filter}' - not a valid ref");
                    return Err(GitError::Exit(128));
                }
            }
        }
        return Ok(());
    }
    let mut matched = false;
    if include_head && let Ok(oid) = resolve_revision(&git_dir, format, "HEAD") {
        matched = true;
        if !quiet {
            print_show_ref(&oid, "HEAD", hash_only, abbrev);
            print_show_ref_deref(&db, format, &oid, "HEAD", dereference, abbrev)?;
        }
    }
    for reference in refs {
        let is_head = reference.name.starts_with("refs/heads/");
        let is_tag = reference.name.starts_with("refs/tags/");
        if (heads || tags) && !((heads && is_head) || (tags && is_tag)) {
            continue;
        }
        if !filters.is_empty()
            && !filters
                .iter()
                .any(|filter| show_ref_filter_matches(&reference.name, filter))
        {
            continue;
        }
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? {
            matched = true;
            if quiet {
                continue;
            }
            print_show_ref(&oid, &reference.name, hash_only, abbrev);
            print_show_ref_deref(&db, format, &oid, &reference.name, dereference, abbrev)?;
        }
    }
    if !filters.is_empty() && !matched {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn parse_show_ref_short_options(
    value: &str,
    quiet: &mut bool,
    hash_only: &mut bool,
    dereference: &mut bool,
    abbrev: &mut Option<usize>,
) -> Result<bool> {
    let Some(flags) = value.strip_prefix('-') else {
        return Ok(false);
    };
    if flags.is_empty() || flags.starts_with('s') {
        return Ok(false);
    }
    for (index, flag) in flags.char_indices() {
        match flag {
            'd' => *dereference = true,
            'q' => *quiet = true,
            's' => {
                *hash_only = true;
                let width = &flags[index + flag.len_utf8()..];
                if !width.is_empty() {
                    *abbrev = Some(parse_abbrev(width)?);
                }
                return Ok(true);
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn show_ref_exists(store: &FileRefStore, refs: &[sley_refs::Ref], name: &str) -> Result<bool> {
    if name == "HEAD" {
        return Ok(store.read_ref("HEAD")?.is_some());
    }
    Ok(refs.iter().any(|reference| reference.name == name))
}

fn show_ref_exists_requires_reference(count: usize) -> Result<()> {
    if count == 0 {
        eprintln!("fatal: --exists requires a reference");
    } else {
        eprintln!("fatal: --exists requires exactly one reference");
    }
    Err(GitError::Exit(128))
}

fn cmd_show_ref_exclude_existing(refs: &[sley_refs::Ref], pattern: Option<&str>) -> Result<()> {
    let existing = refs
        .iter()
        .map(|reference| reference.name.as_str())
        .collect::<HashSet<_>>();
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let mut stdout = io::stdout();
    for line in input.lines() {
        if line.is_empty() {
            continue;
        }
        let (candidate, output) = show_ref_exclude_existing_candidate(line);
        if let Some(pattern) = pattern
            && !candidate.starts_with(pattern)
        {
            continue;
        }
        if !candidate.starts_with("refs/") || validate_ref_name(candidate).is_err() {
            eprintln!("warning: ref '{candidate}' ignored");
            continue;
        }
        if !existing.contains(candidate) {
            stdout.write_all(output.as_bytes())?;
            stdout.write_all(b"\n")?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn show_ref_exclude_existing_candidate(line: &str) -> (&str, &str) {
    let output = line.strip_suffix("^{}").unwrap_or(line);
    let candidate = output
        .rsplit_once(char::is_whitespace)
        .map(|(_, candidate)| candidate)
        .unwrap_or(output);
    (candidate, output)
}

fn print_show_ref(oid: &ObjectId, name: &str, hash_only: bool, abbrev: Option<usize>) {
    let oid = oid.to_hex();
    let display_len = abbrev.unwrap_or(oid.len()).min(oid.len());
    let display_oid = &oid[..display_len];
    if hash_only {
        println!("{display_oid}");
    } else {
        println!("{display_oid} {name}");
    }
}

fn print_show_ref_deref(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    name: &str,
    dereference: bool,
    abbrev: Option<usize>,
) -> Result<()> {
    if !dereference {
        return Ok(());
    }
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Tag {
        return Ok(());
    }
    let peeled = sley_rev::peel_tags(db, format, oid)?;
    print_show_ref(&peeled, &format!("{name}^{{}}"), false, abbrev);
    Ok(())
}

pub(crate) fn cmd_symbolic_ref(args: &[String]) -> Result<()> {
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let specs = symbolic_ref_option_specs();
    let usage = symbolic_ref_usage_lines();
    let parsed = parse_options(args, &specs, &usage).map_err(symbolic_ref_usage_error)?;

    let short = parsed.last_bool("short", false);
    let quiet = parsed.last_bool("quiet", false);
    let recurse = parsed.last_bool("recurse", true);
    let delete = parsed.last_bool("delete", false);
    let message_value = parsed
        .options
        .iter()
        .filter(|option| option.short == Some('m'))
        .filter_map(|option| match option.value {
            ParsedValue::Str(value) => Some(value),
            _ => None,
        })
        .next_back();
    if message_value == Some("") {
        eprintln!("fatal: Refusing to perform update with empty message");
        return Err(GitError::Exit(128));
    }
    let message = message_value.unwrap_or("").as_bytes().to_vec();

    let positional = parsed.positionals;
    if delete {
        return match positional.as_slice() {
            [name] => delete_symbolic_ref(&store, name),
            _ => symbolic_ref_usage(),
        };
    }
    match positional.as_slice() {
        [name] => {
            let target = read_symbolic_ref_target(&store, name, recurse, quiet)?;
            if short {
                println!("{}", symbolic_ref_short_name(&target));
            } else {
                println!("{target}");
            }
            Ok(())
        }
        [name, target] => update_symbolic_ref(&git_dir, &store, format, name, target, message),
        _ => Err(GitError::Command(
            "symbolic-ref currently supports: symbolic-ref [--short] [--quiet] <name> or symbolic-ref <name> <ref>"
                .into(),
        )),
    }
}

fn update_symbolic_ref(
    git_dir: &Path,
    store: &FileRefStore,
    format: ObjectFormat,
    name: &str,
    target: &str,
    message: Vec<u8>,
) -> Result<()> {
    validate_symref_name(name)?;
    if name == "HEAD" && !target.starts_with("refs/") {
        return symbolic_ref_refusing_outside_refs();
    }
    if validate_symref_target(target).is_err() {
        eprintln!("fatal: Refusing to set '{name}' to invalid ref '{target}'");
        return Err(GitError::Exit(128));
    }
    let old_oid = resolve_symbolic_ref_oid(store, format, name)?;
    let new_oid = resolve_symbolic_ref_oid(store, format, target)?;
    let reflog = symbolic_ref_should_write_reflog(git_dir, name)?.then(|| ReflogEntry {
        old_oid,
        new_oid,
        committer: default_committer(),
        message,
    });
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: name.into(),
        expected: None,
        new: RefTarget::Symbolic(target.into()),
        reflog,
    });
    commit_symbolic_ref_update(tx)
}

fn commit_symbolic_ref_update(tx: sley_refs::FileRefTransaction<'_>) -> Result<()> {
    match tx.commit() {
        Ok(()) => Ok(()),
        Err(GitError::Transaction(message)) if message.starts_with("cannot lock ref '") => {
            eprintln!("error: {message}");
            Err(GitError::Exit(1))
        }
        Err(err) => Err(err),
    }
}

fn symbolic_ref_should_write_reflog(git_dir: &Path, name: &str) -> Result<bool> {
    if name == "HEAD" {
        return Ok(true);
    }
    update_ref_should_write_reflog(git_dir, name, false)
}

fn resolve_symbolic_ref_oid(
    store: &FileRefStore,
    format: ObjectFormat,
    name: &str,
) -> Result<ObjectId> {
    match resolve_ref_peeled(store, name)? {
        Some(oid) => Ok(oid),
        None => zero_oid(format),
    }
}

fn read_symbolic_ref_target(
    store: &FileRefStore,
    name: &str,
    recurse: bool,
    quiet: bool,
) -> Result<String> {
    let mut current = name.to_string();
    let mut target = match store.read_ref(&current)? {
        Some(RefTarget::Symbolic(target)) => target,
        _ if quiet => return Err(GitError::Exit(1)),
        _ => return symbolic_ref_not_symbolic(name),
    };
    if !recurse {
        return Ok(target);
    }
    for _ in 0..16 {
        current = target.clone();
        match store.read_ref(&current)? {
            Some(RefTarget::Symbolic(next)) => target = next,
            _ => return Ok(target),
        }
    }
    Ok(target)
}

fn symbolic_ref_not_symbolic(name: &str) -> Result<String> {
    eprintln!("fatal: ref {name} is not a symbolic ref");
    Err(GitError::Exit(128))
}

fn symbolic_ref_short_name(name: &str) -> &str {
    if let Some(remote) = name.strip_prefix("refs/remotes/")
        && let Some(remote_name) = remote.strip_suffix("/HEAD")
    {
        return remote_name;
    }
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/tags/"))
        .or_else(|| name.strip_prefix("refs/remotes/"))
        .or_else(|| name.strip_prefix("refs/"))
        .unwrap_or(name)
}

fn symbolic_ref_refusing_outside_refs() -> Result<()> {
    eprintln!("fatal: Refusing to point HEAD outside of refs/");
    Err(GitError::Exit(128))
}

fn symbolic_ref_usage() -> Result<()> {
    eprint!(
        "{}",
        sley_options::usage_with_options(&symbolic_ref_option_specs(), &symbolic_ref_usage_lines())
    );
    Err(GitError::Exit(129))
}

fn symbolic_ref_option_specs() -> [OptionSpec<'static>; 5] {
    [
        OptionSpec {
            short: Some('q'),
            long: Some("quiet"),
            value: OptValue::Bool,
            flags: OptFlags::NONE,
            help: "suppress error message for non-symbolic (detached) refs",
        },
        OptionSpec {
            short: Some('d'),
            long: Some("delete"),
            value: OptValue::Bool,
            flags: OptFlags::NONE,
            help: "delete symbolic ref",
        },
        OptionSpec {
            short: None,
            long: Some("short"),
            value: OptValue::Bool,
            flags: OptFlags::NONE,
            help: "shorten ref output",
        },
        OptionSpec {
            short: None,
            long: Some("recurse"),
            value: OptValue::Bool,
            flags: OptFlags::NONE,
            help: "recursively dereference (default)",
        },
        OptionSpec {
            short: Some('m'),
            long: None,
            value: OptValue::Str("reason"),
            flags: OptFlags::NONE,
            help: "reason of the update",
        },
    ]
}

fn symbolic_ref_usage_lines() -> [&'static str; 3] {
    [
        "git symbolic-ref [-m <reason>] <name> <ref>",
        "git symbolic-ref [-q] [--short] [--no-recurse] <name>",
        "git symbolic-ref --delete [-q] <name>",
    ]
}

fn symbolic_ref_usage_error(error: UsageError) -> GitError {
    eprint!("{}", error.render_stderr());
    GitError::Exit(error.exit_code())
}

/// `git refs` command group (builtin/refs.c, git 2.54). Dispatches to the ref
/// plumbing subcommands. `list` is an exact clone of `for-each-ref` (it calls
/// the same for_each_ref_core in git); `exists` is a raw ref-existence probe.
/// migrate/verify/optimize are out of scope for the parity quick-win.
pub(crate) fn cmd_refs(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        eprintln!("error: need a subcommand");
        print_refs_usage();
        return Err(GitError::Exit(129));
    };
    match subcommand {
        "list" => commands::for_each_ref::for_each_ref_core(&args[1..], "git refs list"),
        "exists" => cmd_refs_exists(&args[1..]),
        "-h" | "--help" => {
            print_refs_usage();
            Err(GitError::Exit(129))
        }
        other => {
            eprintln!("error: unknown subcommand: `{other}'");
            print_refs_usage();
            Err(GitError::Exit(129))
        }
    }
}

/// `git refs exists <ref>` — exit 0 if the raw ref exists, 2 if it does not
/// (ENOENT/EISDIR), matching builtin/refs.c::cmd_refs_exists. Does not DWIM and
/// does not read the pointed-at object.
fn cmd_refs_exists(args: &[String]) -> Result<()> {
    // git: `argc != 1` after option parsing -> die. There are no options other
    // than -h, which parse_options would have consumed in cmd_refs already.
    let refs: Vec<&String> = args.iter().filter(|arg| arg.as_str() != "--").collect();
    if refs.len() != 1 {
        eprintln!("fatal: 'git refs exists' requires a reference");
        return Err(GitError::Exit(128));
    }
    let name = refs[0];
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    if store.raw_ref_exists(name)? {
        Ok(())
    } else {
        eprintln!("error: reference does not exist");
        Err(GitError::Exit(2))
    }
}

fn print_refs_usage() {
    eprintln!("usage: git refs migrate --ref-format=<format> [--no-reflog] [--dry-run]");
    eprintln!("   or: git refs verify [--strict] [--verbose]");
    eprintln!("   or: git refs list [--count=<count>] [--shell|--perl|--python|--tcl]");
    eprintln!("                                [(--sort=<key>)...] [--format=<format>]");
    eprintln!("                                [--include-root-refs] [--points-at=<object>]");
    eprintln!("                                [--merged[=<object>]] [--no-merged[=<object>]]");
    eprintln!("                                [--contains[=<object>]] [--no-contains[=<object>]]");
    eprintln!(
        "                                [(--exclude=<pattern>)...] [--start-after=<marker>]"
    );
    eprintln!("                                [ --stdin | (<pattern>...)]");
    eprintln!("   or: git refs exists <ref>");
    eprintln!(
        "   or: git refs optimize [--all] [--no-prune] [--auto] [--include <pattern>] [--exclude <pattern>]"
    );
    eprintln!();
}
