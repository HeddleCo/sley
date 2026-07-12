//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use sley::plumbing::{sley_config, sley_formats::ReftableWriteOptions, sley_refs, sley_rev};
use std::cell::RefCell;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_options::{OptFlags, OptValue, OptionSpec, ParsedValue, UsageError, parse_options};

#[path = "refs_options.rs"]
mod refs_options;
use refs_options::{setup_reflog_show_options, setup_show_ref_short_options};

#[derive(Debug)]
pub(super) struct ReflogShowOptions {
    reference: String,
    display: String,
    format: ReflogFormat,
    max_count: Option<usize>,
    abbrev_commit: Option<bool>,
    pathspecs: Vec<String>,
    grep_patterns: Vec<String>,
    grep_pattern_kind: sley_grep::PatternKind,
    grep_pattern_kind_explicit: bool,
    grep_ignore_case: bool,
    grep_all_match: bool,
    grep_invert: bool,
}

#[derive(Debug)]
pub(super) enum ReflogFormat {
    Default,
    NewOid { final_newline: bool },
    Message { final_newline: bool },
}

pub(crate) fn cmd_reflog(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    if args
        .first()
        .is_some_and(|arg| arg == "-h" || arg == "--help")
    {
        print_reflog_usage_stdout();
        return Err(GitError::Exit(129));
    }
    if args
        .get(1)
        .is_some_and(|arg| arg == "-h" || arg == "--help")
    {
        match args.first().map(String::as_str) {
            Some("show") => print_reflog_show_usage_stdout(),
            Some("list") => print_reflog_list_usage_stdout(),
            Some("exists") => print_reflog_exists_usage_stdout(),
            Some("write") => print_reflog_write_usage_stdout(),
            Some("delete") => print_reflog_delete_usage_stdout(),
            Some("drop") => print_reflog_drop_usage_stdout(),
            Some("expire") => print_reflog_expire_usage_stdout(),
            _ => print_reflog_usage_stdout(),
        }
        return Err(GitError::Exit(129));
    }
    if args.first().is_some_and(|arg| arg == "exists") {
        return cmd_reflog_exists(cli_session, &args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "list") {
        return cmd_reflog_list(cli_session, &args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "delete") {
        return cmd_reflog_delete(cli_session, &args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "drop") {
        return cmd_reflog_drop(cli_session, &args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "write") {
        return cmd_reflog_write(cli_session, &args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "expire") {
        return cmd_reflog_expire(cli_session, &args[1..]);
    }
    if args.len() == 1 && args[0] == "--all" {
        return cmd_reflog_all(cli_session);
    }
    let cwd = cli_session.cwd();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let options = setup_reflog_show_options(&store, &git_dir, format, args)?;
    let config = read_repo_config(&git_dir)?;
    let abbrev_commit = options.abbrev_commit.unwrap_or(true);
    let abbrev_len = if abbrev_commit {
        repository_abbrev(&git_dir, format)?
    } else {
        None
    };
    let mut entries = store.read_reflog(&options.reference)?;
    if !options.pathspecs.is_empty() && !reflog_show_pathspecs_match(cwd, &options.pathspecs) {
        return Ok(());
    }
    entries.reverse();
    let grep_kind = log_grep_pattern_kind_from_config(
        &config,
        options.grep_pattern_kind,
        options.grep_pattern_kind_explicit,
    );
    let grep_matcher = compile_log_message_grep_matcher(
        &options.grep_patterns,
        grep_kind,
        options.grep_ignore_case,
    )?;
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
        if let Some(matcher) = grep_matcher.as_ref() {
            let matched = if options.grep_all_match {
                matcher.matches_all(&entry.message)
            } else {
                matcher.matches_any(&entry.message)
            };
            if matched == options.grep_invert {
                continue;
            }
        }
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
                format_log_oid(&entry.new_oid, abbrev_len),
                options.display,
                shown,
                String::from_utf8_lossy(&reflog_show_message(entry))
            ),
            ReflogFormat::NewOid { final_newline } => {
                if final_newline || shown + 1 < selected.len() {
                    println!("{}", entry.new_oid);
                } else {
                    print!("{}", entry.new_oid);
                }
            }
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

fn reflog_show_message(entry: &ReflogEntry) -> std::borrow::Cow<'_, [u8]> {
    if entry.old_oid.is_null()
        && let Some(rest) = entry.message.strip_prefix(b"commit: ")
    {
        let mut message = b"commit (initial): ".to_vec();
        message.extend_from_slice(rest);
        return std::borrow::Cow::Owned(message);
    }
    std::borrow::Cow::Borrowed(&entry.message)
}

fn cmd_reflog_all(cli_session: &crate::session::CliSession) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let abbrev_len = repository_abbrev(&git_dir, format)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut names = store.list_reflog_names()?;
    names.sort();
    for name in names {
        let mut entries = store.read_reflog(&name)?;
        entries.reverse();
        for (shown, entry) in entries.iter().enumerate() {
            println!(
                "{} {}@{{{}}}: {}",
                format_log_oid(&entry.new_oid, abbrev_len),
                name,
                shown,
                String::from_utf8_lossy(&entry.message)
            );
        }
    }
    Ok(())
}

fn print_reflog_usage_stdout() {
    println!("usage: git reflog [show] [<log-options>] [<ref>]");
    println!("   or: git reflog list");
    println!("   or: git reflog exists <ref>");
    println!("   or: git reflog write <ref> <old-oid> <new-oid> <message>");
    println!("   or: git reflog delete [--rewrite] [--updateref]");
    println!("                         [--dry-run | -n] [--verbose] <ref>@{{<specifier>}}...");
    println!("   or: git reflog drop [--all [--single-worktree] | <refs>...]");
    println!("   or: git reflog expire [--expire=<time>] [--expire-unreachable=<time>]");
    println!("                         [--rewrite] [--updateref] [--stale-fix]");
    println!(
        "                         [--dry-run | -n] [--verbose] [--all [--single-worktree] | <refs>...]"
    );
}

fn print_reflog_show_usage_stdout() {
    println!("usage: git reflog [show] [<log-options>] [<ref>]");
}

fn print_reflog_list_usage_stdout() {
    println!("usage: git reflog list");
}

fn print_reflog_exists_usage_stdout() {
    println!("usage: git reflog exists <ref>");
}

fn print_reflog_write_usage_stdout() {
    println!("usage: git reflog write <ref> <old-oid> <new-oid> <message>");
}

fn print_reflog_delete_usage_stdout() {
    println!("usage: git reflog delete [--rewrite] [--updateref]");
    println!("                         [--dry-run | -n] [--verbose] <ref>@{{<specifier>}}...");
}

fn print_reflog_drop_usage_stdout() {
    println!("usage: git reflog drop [--all [--single-worktree] | <refs>...]");
}

fn print_reflog_expire_usage_stdout() {
    println!("usage: git reflog expire [--expire=<time>] [--expire-unreachable=<time>]");
    println!("                         [--rewrite] [--updateref] [--stale-fix]");
    println!(
        "                         [--dry-run | -n] [--verbose] [--all [--single-worktree] | <refs>...]"
    );
}

fn cmd_reflog_exists(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let Some(reference) = args.first() else {
        eprintln!("usage: git reflog exists <ref>");
        eprintln!();
        return Err(GitError::Exit(129));
    };
    let git_dir = cli_session.git_dir()?;
    if reflog_path_for_ref(&git_dir, reference)?.is_file() {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

fn cmd_reflog_list(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
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

    let git_dir = cli_session.git_dir()?;
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
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(git_dir, format);
    if store.uses_reftable()? {
        names.extend(store.list_reflog_names()?);
        return Ok(());
    }
    let common_logs = common_git_dir.join("logs");
    let worktree_logs = git_dir.join("logs");
    if worktree_logs != common_logs {
        let mut common_names = BTreeSet::new();
        collect_reflog_names(&common_logs, &common_logs, &mut common_names)?;
        names.extend(
            common_names
                .into_iter()
                .filter(|name| !name.starts_with("refs/worktree/")),
        );
        collect_reflog_names(&worktree_logs, &worktree_logs, names)?;
    } else {
        collect_reflog_names(&common_logs, &common_logs, names)?;
    }
    Ok(())
}

fn collect_repository_reflog_targets(
    git_dir: &Path,
    targets: &mut BTreeSet<(PathBuf, String)>,
) -> Result<()> {
    let mut names = BTreeSet::new();
    collect_repository_reflog_names(git_dir, &mut names)?;
    targets.extend(names.into_iter().map(|name| (git_dir.to_path_buf(), name)));
    Ok(())
}

fn collect_current_worktree_reflog_targets(
    git_dir: &Path,
    targets: &mut BTreeSet<(PathBuf, String)>,
) -> Result<()> {
    collect_repository_reflog_targets(git_dir, targets)
}

fn collect_all_worktree_reflog_targets(
    git_dir: &Path,
    targets: &mut BTreeSet<(PathBuf, String)>,
) -> Result<()> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    collect_repository_reflog_targets(&common_git_dir, targets)?;
    let worktrees_dir = common_git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        collect_repository_reflog_targets(&entry.path(), targets)?;
    }
    Ok(())
}

fn reflog_path_for_ref(git_dir: &Path, name: &str) -> Result<PathBuf> {
    Ok(reflog_logs_dir_for_ref(git_dir, name)?.join(name))
}

fn loose_ref_path_for_ref(git_dir: &Path, name: &str) -> Result<PathBuf> {
    if name == "HEAD" || name.starts_with("refs/worktree/") {
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
    if name == "HEAD" || name.starts_with("refs/worktree/") {
        Ok(git_dir.join("logs"))
    } else {
        Ok(common_git_dir_for_git_dir(git_dir)?.join("logs"))
    }
}

#[derive(Debug, Clone, Copy)]
struct ReflogDeleteOptions {
    dry_run: bool,
    verbose: bool,
    rewrite: bool,
    update_ref: bool,
}

#[derive(Debug, Clone, Copy)]
struct ReflogExpireOptions {
    dry_run: bool,
    verbose: bool,
    rewrite: bool,
    update_ref: bool,
    all: bool,
    single_worktree: bool,
    stale_fix: bool,
    expire: i64,
    expire_unreachable: i64,
    explicit_expire: bool,
    explicit_expire_unreachable: bool,
}

#[derive(Debug)]
struct ReflogExpireRunContext {
    config: GitConfig,
    reachable_by_tip: HashMap<ObjectId, HashSet<ObjectId>>,
}

#[derive(Debug, Clone, Copy)]
struct ReflogDropOptions {
    all: bool,
}

fn cmd_reflog_delete(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut options = ReflogDeleteOptions {
        dry_run: false,
        verbose: false,
        rewrite: false,
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
            "--rewrite" => options.rewrite = true,
            "--no-rewrite" => options.rewrite = false,
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

    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut exit_code = 0;
    for spec in specs {
        if let Err(GitError::Exit(code)) =
            delete_reflog_entry(&store, &git_dir, format, &spec, options)
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
    git_dir: &Path,
    format: ObjectFormat,
    spec: &str,
    options: ReflogDeleteOptions,
) -> Result<()> {
    let Some((reference, selector)) = parse_reflog_delete_spec(store, git_dir, format, spec) else {
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
                if options.dry_run {
                    "would prune"
                } else {
                    "prune"
                }
            } else {
                "keep"
            };
            println!("{action} {}", String::from_utf8_lossy(&entry.message));
        }
    }
    if !options.dry_run {
        let old_tip = entries.last().map(|entry| entry.new_oid);
        entries.remove(delete_index);
        if options.rewrite {
            rewrite_reflog_old_oids(&mut entries, zero_oid(format)?);
        }
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

fn parse_reflog_delete_spec(
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    spec: &str,
) -> Option<(String, usize)> {
    let spec = spec.strip_suffix('}')?;
    let (reference, selector) = spec.rsplit_once("@{")?;
    let reference = resolve_reflog_name(store, git_dir, format, reference).ok()?;
    let selector = selector.parse::<usize>().ok().or_else(|| {
        reflog_selector_by_date(store, &reference, selector)
            .ok()
            .flatten()
    })?;
    Some((reference, selector))
}

fn reflog_selector_by_date(
    store: &FileRefStore,
    reference: &str,
    selector: &str,
) -> Result<Option<usize>> {
    let Some(cutoff) = crate::commands::approxidate::parse_approxidate(selector)
        .or_else(|| parse_reflog_expire_date(selector))
    else {
        return Ok(None);
    };
    let entries = store.read_reflog(reference)?;
    Ok(entries
        .iter()
        .rev()
        .position(|entry| entry.timestamp_seconds().is_ok_and(|ts| ts <= cutoff)))
}

fn rewrite_reflog_old_oids(entries: &mut [ReflogEntry], zero: ObjectId) {
    let mut previous = zero;
    for entry in entries {
        entry.old_oid = previous;
        previous = entry.new_oid;
    }
}

fn cmd_reflog_drop(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
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

    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
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
        let reference = reflog_reference_name(&store, &git_dir, format, Some(&reference))?;
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

fn cmd_reflog_write(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    if args.len() != 4 {
        eprintln!("usage: git reflog write <ref> <old-oid> <new-oid> <message>");
        eprintln!();
        return Err(GitError::Exit(129));
    }
    let reference = &args[0];
    if !reflog_write_refname_is_valid(reference) {
        eprintln!("fatal: invalid reference name: {reference}");
        return Err(GitError::Exit(128));
    }

    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let old_oid = parse_reflog_write_oid(format, &args[1], "old")?;
    let new_oid = parse_reflog_write_oid(format, &args[2], "new")?;
    let zero = zero_oid(format)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    validate_reflog_write_object(&db, &old_oid, &zero, "old")?;
    validate_reflog_write_object(&db, &new_oid, &zero, "new")?;

    let store = FileRefStore::new(&git_dir, format);
    let identity_config = identity_effective_config_for(cli_session).unwrap_or_default();
    store.append_reflog(
        reference,
        &ReflogEntry {
            old_oid,
            new_oid,
            committer: commit_identity_from_env("COMMITTER", &identity_config)?,
            message: normalize_reflog_write_message(&args[3]),
        },
    )
}

fn reflog_write_refname_is_valid(reference: &str) -> bool {
    validate_ref_name(reference).is_ok()
        || (!reference.starts_with("refs/") && sley_refs::refname_is_safe(reference))
}

fn normalize_reflog_write_message(message: &str) -> Vec<u8> {
    message
        .split(|ch: char| ch == '\n' || ch == '\r')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .into_bytes()
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

fn cmd_reflog_expire(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let (options, refs) = parse_reflog_expire_options(args)?;
    let git_dir = cli_session.git_dir()?;
    expire_reflogs_at(&git_dir, options, refs, cli_session.replace_objects())
}

/// Path-based reflog expiration for repository maintenance that already owns
/// repository discovery and must not re-enter CLI global state.
pub(crate) fn reflog_expire_at(
    git_dir: &Path,
    args: &[String],
    replace_objects: bool,
) -> Result<()> {
    let (options, refs) = parse_reflog_expire_options(args)?;
    expire_reflogs_at(git_dir, options, refs, replace_objects)
}

fn parse_reflog_expire_options(args: &[String]) -> Result<(ReflogExpireOptions, Vec<String>)> {
    let mut options = ReflogExpireOptions {
        dry_run: false,
        verbose: false,
        rewrite: false,
        update_ref: false,
        all: false,
        single_worktree: false,
        stale_fix: false,
        expire: current_unix_seconds().saturating_sub(90 * 24 * 60 * 60),
        expire_unreachable: current_unix_seconds().saturating_sub(30 * 24 * 60 * 60),
        explicit_expire: false,
        explicit_expire_unreachable: false,
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
            "--stale-fix" => options.stale_fix = true,
            "--no-stale-fix" => options.stale_fix = false,
            "--all" => options.all = true,
            "--no-all" => options.all = false,
            "--single-worktree" => options.single_worktree = true,
            "--no-single-worktree" => options.single_worktree = false,
            "--expire" | "--expire-unreachable" => {
                let Some(value) = args.next_value() else {
                    return reflog_expire_option_requires_value(arg.trim_start_matches("--"));
                };
                let cutoff = parse_reflog_expire_time(value, arg)?;
                if arg == "--expire" {
                    options.expire = cutoff;
                    options.explicit_expire = true;
                } else {
                    options.expire_unreachable = cutoff;
                    options.explicit_expire_unreachable = true;
                }
            }
            value if let Some(time) = long_option_value(value, "expire") => {
                options.expire = parse_reflog_expire_time(time, "--expire")?;
                options.explicit_expire = true;
            }
            value if let Some(time) = long_option_value(value, "expire-unreachable") => {
                options.expire_unreachable =
                    parse_reflog_expire_time(time, "--expire-unreachable")?;
                options.explicit_expire_unreachable = true;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return reflog_expire_usage();
            }
            value => refs.push(value.to_string()),
        }
    }
    Ok((options, refs))
}

fn expire_reflogs_at(
    git_dir: &Path,
    mut options: ReflogExpireOptions,
    refs: Vec<String>,
    replace_objects: bool,
) -> Result<()> {
    let format = repository_object_format(git_dir)?;
    let store = FileRefStore::new(git_dir, format);
    let config = load_reflog_expire_config(git_dir)?;
    apply_reflog_expire_config_from(&config, &mut options)?;
    let mut targets: BTreeSet<(PathBuf, String)> = BTreeSet::new();
    // References discovered via `--all` silently skip an empty/missing reflog
    // (git's `reflog expire --all` only walks reflogs that exist); explicit
    // references still error on a missing reflog.
    let mut all_discovered: BTreeSet<(PathBuf, String)> = BTreeSet::new();
    if options.all {
        if options.single_worktree {
            collect_current_worktree_reflog_targets(git_dir, &mut all_discovered)?;
        } else {
            collect_all_worktree_reflog_targets(git_dir, &mut all_discovered)?;
        }
        targets.extend(all_discovered.iter().cloned());
    }
    for original in refs {
        if is_reflog_selector(&original) {
            eprintln!("error: reflog could not be found: '{original}'");
            return Err(GitError::Exit(255));
        }
        let reference = resolve_reflog_name(&store, git_dir, format, &original).map_err(|_| {
            eprintln!("error: reflog could not be found: '{original}'");
            GitError::Exit(255)
        })?;
        if store.read_reflog(&reference)?.is_empty() {
            eprintln!("error: reflog could not be found: '{original}'");
            return Err(GitError::Exit(255));
        }
        targets.insert((git_dir.to_path_buf(), reference));
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut context = ReflogExpireRunContext {
        config,
        reachable_by_tip: HashMap::new(),
    };
    let mut exit_code = 0;
    for (target_git_dir, reference) in targets {
        let target_store = if target_git_dir == git_dir {
            store.clone()
        } else {
            FileRefStore::new(&target_git_dir, format)
        };
        // A `--all`-discovered reflog that is empty is not an error.
        let target = (target_git_dir.clone(), reference.clone());
        let discovered = all_discovered.contains(&target);
        if discovered && target_store.read_reflog_for_expiry(&reference)?.is_empty() {
            continue;
        }
        let mut target_options = options;
        apply_reflog_expire_pattern_config_from(&context.config, &reference, &mut target_options)?;
        if let Err(GitError::Exit(code)) = expire_reflog_entries(
            &target_store,
            &db,
            &target_git_dir,
            format,
            &reference,
            target_options,
            replace_objects,
            &mut context,
            discovered,
        ) {
            exit_code = code;
        }
    }
    if exit_code == 0 {
        Ok(())
    } else {
        Err(GitError::Exit(exit_code))
    }
}

fn is_reflog_selector(value: &str) -> bool {
    value
        .strip_suffix('}')
        .is_some_and(|prefix| prefix.contains("@{"))
}

fn expire_reflog_entries(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    reference: &str,
    options: ReflogExpireOptions,
    replace_objects: bool,
    context: &mut ReflogExpireRunContext,
    lenient: bool,
) -> Result<()> {
    let mut entries = if lenient {
        store.read_reflog_for_expiry(reference)?
    } else {
        store.read_reflog(reference)?
    };
    if entries.is_empty() {
        eprintln!("error: reflog could not be found: '{reference}'");
        return Err(GitError::Exit(255));
    }
    let zero = zero_oid(format)?;
    let mut retained = Vec::new();
    let mut last_kept = zero.clone();
    let mut reachable = None::<Option<HashSet<ObjectId>>>;
    let mut rewrote_old_oid = false;
    for entry in &entries {
        let mut entry = entry.clone();
        if options.rewrite {
            entry.old_oid = last_kept;
            rewrote_old_oid = true;
        }
        let timestamp = entry.timestamp_seconds()?;
        let stale = options.stale_fix
            && (!reflog_oid_has_complete_commit(db, format, &entry.old_oid, &zero)
                || !reflog_oid_has_complete_commit(db, format, &entry.new_oid, &zero));
        let mut prune = timestamp < options.expire || stale;
        if !prune && timestamp < options.expire_unreachable {
            if reachable.is_none() {
                reachable = Some(reflog_reachable_oids(
                    store,
                    db,
                    git_dir,
                    format,
                    reference,
                    options,
                    replace_objects,
                    context,
                )?);
            }
            let reachable_from_tip = reachable
                .as_ref()
                .and_then(|value| value.as_ref())
                .is_none_or(|commits| {
                    reflog_oid_is_reachable_or_non_commit(db, &entry.old_oid, &zero, commits)
                        && reflog_oid_is_reachable_or_non_commit(db, &entry.new_oid, &zero, commits)
                });
            prune = !reachable_from_tip;
        }
        if options.verbose {
            let action = if prune {
                if options.dry_run {
                    "would prune"
                } else {
                    "prune"
                }
            } else {
                "keep"
            };
            println!("{action} {}", String::from_utf8_lossy(&entry.message));
        }
        if !prune {
            last_kept = entry.new_oid;
            retained.push(entry);
        }
    }
    if options.dry_run {
        return Ok(());
    }
    if retained.len() == entries.len() && !rewrote_old_oid {
        store.adjust_reflog_permissions(reference)?;
        return Ok(());
    }
    let old_tip = entries.last().map(|entry| entry.new_oid);
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

fn resolve_reflog_name(
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    name: &str,
) -> Result<String> {
    let direct = reflog_reference_name(store, git_dir, format, Some(name))?;
    if !store.read_reflog(&direct)?.is_empty() {
        return Ok(direct);
    }
    if !name.starts_with("refs/") && name != "HEAD" {
        let branch = format!("refs/heads/{name}");
        if !store.read_reflog(&branch)?.is_empty() {
            return Ok(branch);
        }
    }
    Ok(direct)
}

fn load_reflog_expire_config(git_dir: &Path) -> Result<GitConfig> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let context = sley_config::ConfigIncludeContext::new(
        Some(common_git_dir.clone()),
        repo_current_branch_name(git_dir),
    );
    sley_config::load_effective_config(&common_git_dir, &context)
}

fn apply_reflog_expire_config_from(
    config: &GitConfig,
    options: &mut ReflogExpireOptions,
) -> Result<()> {
    if !options.explicit_expire
        && let Some(value) = config.get("gc", None, "reflogExpire")
    {
        options.expire = parse_reflog_expire_time(value, "gc.reflogExpire")?;
    }
    if !options.explicit_expire_unreachable
        && let Some(value) = config.get("gc", None, "reflogExpireUnreachable")
    {
        options.expire_unreachable = parse_reflog_expire_time(value, "gc.reflogExpireUnreachable")?;
    }
    Ok(())
}

fn apply_reflog_expire_pattern_config_from(
    config: &GitConfig,
    reference: &str,
    options: &mut ReflogExpireOptions,
) -> Result<()> {
    if options.explicit_expire && options.explicit_expire_unreachable {
        return Ok(());
    }
    if reference == "refs/stash" {
        if !options.explicit_expire {
            options.expire = i64::MIN;
        }
        if !options.explicit_expire_unreachable {
            options.expire_unreachable = i64::MIN;
        }
    }
    for section in config.sections.iter().rev() {
        if !section.name.eq_ignore_ascii_case("gc") {
            continue;
        }
        let Some(pattern) = section.subsection.as_deref() else {
            continue;
        };
        if !reflog_expire_pattern_matches(pattern, reference) {
            continue;
        }
        for entry in section.entries.iter().rev() {
            if !options.explicit_expire && entry.key.eq_ignore_ascii_case("reflogExpire") {
                if let Some(value) = entry.value.as_deref() {
                    options.expire = parse_reflog_expire_time(value, "gc.*.reflogExpire")?;
                }
            } else if !options.explicit_expire_unreachable
                && entry.key.eq_ignore_ascii_case("reflogExpireUnreachable")
                && let Some(value) = entry.value.as_deref()
            {
                options.expire_unreachable =
                    parse_reflog_expire_time(value, "gc.*.reflogExpireUnreachable")?;
            }
        }
        break;
    }
    Ok(())
}

fn reflog_expire_pattern_matches(pattern: &str, reference: &str) -> bool {
    if pattern == reference {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return reference.starts_with(prefix);
    }
    false
}

fn reflog_reachable_oids(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    reference: &str,
    options: ReflogExpireOptions,
    replace_objects: bool,
    context: &mut ReflogExpireRunContext,
) -> Result<Option<HashSet<ObjectId>>> {
    if options.expire_unreachable <= options.expire {
        return Ok(Some(HashSet::new()));
    }
    let mut starts = Vec::new();
    if reference == "HEAD" {
        for reference in store.list_refs()? {
            if let Some(oid) = resolve_ref_to_oid(store, &reference.name)? {
                starts.push(oid);
            }
        }
    } else if let Some(tip) = resolve_ref_to_oid(store, reference)? {
        starts.push(tip);
    } else if let Ok(tip) = resolve_revision(git_dir, format, reference, replace_objects) {
        starts.push(tip);
    } else {
        return Ok(Some(HashSet::new()));
    }
    let mut reachable = HashSet::new();
    for start in starts {
        if !context.reachable_by_tip.contains_key(&start) {
            let tip_reachable = match sley_rev::ancestor_depths(git_dir, format, db, &start) {
                Ok(depths) => depths.into_keys().collect(),
                Err(_) => HashSet::new(),
            };
            context.reachable_by_tip.insert(start, tip_reachable);
        }
        if let Some(tip_reachable) = context.reachable_by_tip.get(&start) {
            reachable.extend(tip_reachable.iter().copied());
        }
    }
    Ok(Some(reachable))
}

fn reflog_oid_is_reachable_or_non_commit(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    zero: &ObjectId,
    reachable: &HashSet<ObjectId>,
) -> bool {
    if oid == zero {
        return true;
    }
    match db.read_object(oid) {
        Ok(object) if object.object_type == ObjectType::Commit => reachable.contains(oid),
        Ok(_) => true,
        Err(_) => false,
    }
}

fn reflog_oid_has_complete_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    zero: &ObjectId,
) -> bool {
    if oid == zero {
        return true;
    }
    let mut seen = HashSet::new();
    reflog_oid_has_complete_commit_inner(db, format, oid, &mut seen)
}

fn reflog_oid_has_complete_commit_inner(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    seen: &mut HashSet<ObjectId>,
) -> bool {
    if !seen.insert(*oid) {
        return true;
    }
    let Ok(object) = db.read_object(oid) else {
        return false;
    };
    if object.object_type != ObjectType::Commit {
        return true;
    }
    let Ok(commit) = Commit::parse_ref(format, &object.body) else {
        return false;
    };
    reflog_tree_is_complete(db, format, &commit.tree, seen)
        && commit
            .parents
            .iter()
            .all(|parent| reflog_oid_has_complete_commit_inner(db, format, parent, seen))
}

fn reflog_tree_is_complete(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    seen: &mut HashSet<ObjectId>,
) -> bool {
    if !seen.insert(*oid) {
        return true;
    }
    let Ok(object) = db.read_object(oid) else {
        return false;
    };
    if object.object_type != ObjectType::Tree {
        return true;
    }
    let entries = TreeEntries::new(format, &object.body);
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        if entry.is_gitlink() {
            continue;
        }
        let Ok(child) = db.read_object(&entry.oid) else {
            return false;
        };
        if child.object_type == ObjectType::Tree
            && !reflog_tree_is_complete(db, format, &entry.oid, seen)
        {
            return false;
        }
    }
    true
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

fn reflog_show_pathspecs_match(cwd: &Path, pathspecs: &[String]) -> bool {
    pathspecs.iter().any(|pathspec| cwd.join(pathspec).exists())
}

const UPDATE_SERVER_INFO_USAGE: &[&str] = &["git update-server-info"];

fn update_server_info_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[OptionSpec {
        short: Some('f'),
        long: Some("force"),
        value: OptValue::Bool,
        flags: OptFlags::NONE,
        help: "force update",
    }];
    SPECS
}

pub(crate) fn cmd_update_server_info(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    update_server_info_at(&git_dir, args)
}

pub(crate) fn update_server_info_at(git_dir: &Path, args: &[String]) -> Result<()> {
    let force = setup_update_server_info_options(args)?;
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let shared_repository =
        sley::plumbing::sley_formats::SharedRepositoryPermissions::from_git_dir(&common_git_dir);

    let info_dir = common_git_dir.join("info");
    shared_repository.create_dir_all(&info_dir)?;
    update_server_info_file(
        &info_dir.join("refs"),
        &update_server_info_refs(&store, &db, format)?,
        force,
        &shared_repository,
    )?;

    let objects_info_dir = repository_objects_dir(&common_git_dir).join("info");
    shared_repository.create_dir_all(&objects_info_dir)?;
    update_server_info_file(
        &objects_info_dir.join("packs"),
        &update_server_info_packs(
            &repository_objects_dir(&common_git_dir).join("pack"),
            format,
        )?,
        force,
        &shared_repository,
    )?;
    Ok(())
}

fn setup_update_server_info_options(args: &[String]) -> Result<bool> {
    let parsed = match parse_options(
        args,
        update_server_info_option_specs(),
        UPDATE_SERVER_INFO_USAGE,
    ) {
        Ok(parsed) => parsed,
        Err(error) => {
            if let Some(message) = error.message() {
                if message.contains("takes no value") {
                    eprintln!("error: {message}");
                    return Err(GitError::Exit(129));
                }
                if message.starts_with("unknown option `") {
                    let option = message
                        .strip_prefix("unknown option `")
                        .and_then(|rest| rest.strip_suffix('\''))
                        .unwrap_or(message);
                    eprintln!("error: unknown option `{option}'");
                } else if message.starts_with("unknown switch `") {
                    let option = message
                        .strip_prefix("unknown switch `")
                        .and_then(|rest| rest.strip_suffix('\''))
                        .unwrap_or(message);
                    eprintln!("error: unknown switch `{option}'");
                }
            }
            return update_server_info_usage();
        }
    };
    if !parsed.positionals.is_empty() {
        return update_server_info_usage();
    }
    Ok(parsed.last_bool("force", false))
}

fn update_server_info_file(
    path: &Path,
    content: &[u8],
    force: bool,
    shared_repository: &sley::plumbing::sley_formats::SharedRepositoryPermissions,
) -> Result<()> {
    if force || !fs::read(path).is_ok_and(|existing| existing == content) {
        fs::write(path, content)?;
    }
    shared_repository.adjust_file(path)
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

pub(crate) fn cmd_update_ref(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    // git's `update-ref` writes an *empty* reflog message when no -m is given
    // (builtin/update-ref.c leaves msg NULL); only -m supplies one.
    let mut message = Vec::new();
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
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let identity_config = identity_effective_config_for(cli_session).unwrap_or_default();
    let core_fsync = global_config_value("core.fsync")?;
    let core_fsync_method = global_config_value("core.fsyncMethod")?;
    let prefer_symlink_refs = global_config_value("core.preferSymlinkRefs")?
        .as_deref()
        .and_then(sley_config::parse_config_bool)
        .or_else(|| identity_config.get_bool("core", None, "preferSymlinkRefs"));
    let mut store = FileRefStore::new(&git_dir, format)
        .with_reference_fsync_config(core_fsync.as_deref(), core_fsync_method.as_deref())
        .with_packed_refs_lock_timeout_millis(packed_refs_lock_timeout_override()?)
        .with_reftable_lock_timeout_millis(reftable_lock_timeout_override()?)
        .with_reftable_write_options(reftable_write_options_override()?)
        // `update-ref` is a native ref transaction: its ref and reflog records
        // share one reftable update index. Other command families still opt in
        // independently while their combined-table parity is completed.
        .with_reftable_combined_logs(true);
    if let Some(prefer_symlink_refs) = prefer_symlink_refs {
        store = store.with_prefer_symlink_refs(prefer_symlink_refs);
    }
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
            config: &identity_config,
            oid_cache: RefCell::new(HashMap::new()),
            object_type_cache: RefCell::new(HashMap::new()),
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
        let effective = match update_ref_effective_ref(&store, &positional[0], deref) {
            Ok(effective) => effective,
            Err(GitError::InvalidPath(_)) => {
                eprintln!(
                    "error: refusing to update ref with bad name '{}'",
                    positional[0]
                );
                return Err(GitError::Exit(1));
            }
            // A name whose stored content does not parse as a ref (an arbitrary
            // file such as `.git/my-private-file`) is not a symref we can follow;
            // git resolves the chain leniently and then validates the final name,
            // so fall through to the safety gate below rather than surfacing the
            // raw parse error.
            Err(GitError::InvalidFormat(_)) => EffectiveRefName {
                requested: positional[0].clone(),
                effective: positional[0].clone(),
            },
            Err(err) => return Err(err),
        };
        // git's delete-time gate (`transaction_refname_valid`, null new-oid):
        // the effective ref name must be `refname_is_safe` — under refs/ or an
        // uppercase/underscore pseudo-ref. A one-level name like
        // `my-private-file` is creatable but NOT deletable, and `update-ref -d`
        // must not be a way to unlink loose files in `.git`.
        if !sley_refs::refname_is_safe(&effective.effective) {
            eprintln!(
                "error: refusing to update ref with bad name '{}'",
                effective.effective
            );
            return Err(GitError::Exit(1));
        }
        if deref
            && effective.requested == effective.effective
            && matches!(
                store.read_ref(&effective.requested)?,
                Some(RefTarget::Symbolic(target)) if target == effective.requested
            )
        {
            eprintln!(
                "error: multiple updates for '{}' (including one via symref '{}') are not allowed",
                effective.requested, effective.requested
            );
            return Err(GitError::Exit(1));
        }
        return update_ref_delete(
            &store,
            &git_dir,
            format,
            &identity_config,
            &effective.effective,
            expected_oid.as_ref(),
            &message,
            create_reflog,
        );
    }
    if positional.len() != 2 && positional.len() != 3 {
        return Err(GitError::Command(
            "update-ref requires <ref> <new-oid> [<old-oid>] or -d <ref>".into(),
        ));
    }
    let requested_name = positional[0].clone();
    let name = match update_ref_effective_name(&store, &requested_name, deref) {
        Ok(name) => name,
        Err(GitError::InvalidPath(_)) => {
            eprintln!(
                "fatal: update_ref failed for ref '{requested_name}': refusing to update ref with bad name '{requested_name}'"
            );
            return Err(GitError::Exit(128));
        }
        Err(err) => return Err(err),
    };
    if sley_refs::validate_ref_name_for_update(&name).is_err() {
        eprintln!(
            "fatal: update_ref failed for ref '{requested_name}': refusing to update ref with bad name '{name}'"
        );
        return Err(GitError::Exit(128));
    }
    let new_oid = parse_update_ref_new_oid(&git_dir, format, &store, &positional[1])?;
    let expected_oid = if let Some(old) = positional.get(2) {
        Some(parse_update_ref_expected(&git_dir, format, &store, old)?)
    } else {
        None
    };
    check_update_ref_new_value(&git_dir, format, &name, &requested_name, &new_oid).map_err(
        |reason| {
            eprintln!("fatal: update_ref failed for ref '{requested_name}': {reason}");
            GitError::Exit(128)
        },
    )?;
    let current = store.read_ref(&name)?;
    if let Some(expected_oid) = expected_oid.as_ref() {
        check_update_ref_expected(format, &name, current.as_ref(), expected_oid)?;
    }
    if new_oid == zero_oid(format)? {
        return update_ref_delete(
            &store,
            &git_dir,
            format,
            &identity_config,
            &name,
            None,
            &message,
            create_reflog,
        );
    }
    let old_oid = match current {
        Some(RefTarget::Direct(oid)) => oid,
        _ => zero_oid(format)?,
    };
    let writes_reflog = update_ref_should_write_reflog(&git_dir, &name, create_reflog)?;
    let reftable_fsync_count = if store.uses_reftable()? {
        2 + if writes_reflog { 2 } else { 0 }
    } else {
        0
    };
    let reflog = writes_reflog.then(|| ReflogEntry {
        old_oid,
        new_oid,
        committer: ref_reflog_committer(&identity_config),
        message,
    });
    let tx_name = name.clone();
    let hook = ReferenceTransactionHookRunner::new(&git_dir);
    let mut tx = store.transaction().with_hook(&hook);
    tx.update(RefUpdate {
        name,
        // The old value was already checked above (and its rejection messages
        // are shaped there); keep the transaction precondition `None` so this
        // path's error surface is unchanged. The hook old-value is reported via
        // `hook_old` below.
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog,
    });
    match tx.commit() {
        Ok(()) => {
            trace_reference_fsync_counter(reftable_fsync_count);
            Ok(())
        }
        Err(GitError::Io(message))
            if message.starts_with(&format!("could not lock ref {tx_name}: ")) =>
        {
            let prefix = format!("could not lock ref {tx_name}: ");
            update_ref_lock_failure(&tx_name, message.trim_start_matches(&prefix))
        }
        Err(GitError::InvalidFormat(message)) if message == "entry too large" => {
            eprintln!(
                "fatal: update_ref failed for ref '{tx_name}': reftable: transaction failure: entry too large"
            );
            Err(GitError::Exit(128))
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

fn reftable_lock_timeout_override() -> Result<Option<u64>> {
    Ok(global_config_value("reftable.lockTimeout")?.and_then(|value| value.parse::<u64>().ok()))
}

fn packed_refs_lock_timeout_override() -> Result<u64> {
    Ok(global_config_value("core.packedRefsTimeout")?
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000))
}

fn reftable_write_options_override() -> Result<ReftableWriteOptions> {
    let mut options = ReftableWriteOptions::default();
    if let Some(value) = global_config_value("reftable.blockSize")?
        && let Ok(block_size) = value.parse::<u32>()
    {
        options.block_size = block_size;
    }
    if let Some(value) = global_config_value("reftable.restartInterval")?
        && let Ok(restart_interval) = value.parse::<u16>()
    {
        options.restart_interval = restart_interval;
    }
    if let Some(value) = global_config_value("reftable.indexObjects")?
        && let Some(index_objects) = sley_config::parse_config_bool(&value)
    {
        options.index_objects = index_objects;
    }
    Ok(options)
}

/// The `reference-transaction` hook runner the file backend fires at each
/// transaction phase. Holds the repo's git dir so it can locate
/// `$GIT_DIR/hooks/reference-transaction`. One instance is created per write and
/// handed to [`FileRefTransaction::with_hook`], so every ref-write path —
/// `update-ref`, `symbolic-ref`, `update-ref --stdin`, push — shares the same
/// firing logic.
pub(crate) struct ReferenceTransactionHookRunner {
    git_dir: PathBuf,
}

impl ReferenceTransactionHookRunner {
    pub(crate) fn new(git_dir: &Path) -> Self {
        Self {
            git_dir: git_dir.to_path_buf(),
        }
    }
}

impl ReferenceTransactionHook for ReferenceTransactionHookRunner {
    fn run(
        &self,
        phase: RefTransactionPhase,
        updates: &[RefTransactionHookUpdate],
    ) -> Result<bool> {
        // Feed one `<old> SP <new> SP <refname> LF` line per update, exactly as
        // git's transaction_hook_feed_stdin builds them.
        let mut stdin = Vec::new();
        for update in updates {
            stdin.extend_from_slice(update.old_value.as_bytes());
            stdin.push(b' ');
            stdin.extend_from_slice(update.new_value.as_bytes());
            stdin.push(b' ');
            stdin.extend_from_slice(update.refname.as_bytes());
            stdin.push(b'\n');
        }
        crate::commands::hooks::run_reference_transaction_hook_at(
            &self.git_dir,
            phase.as_str(),
            stdin,
        )
    }
}

struct UpdateRefStdinContext<'a> {
    git_dir: &'a Path,
    store: &'a FileRefStore,
    format: ObjectFormat,
    create_reflog: bool,
    message: Vec<u8>,
    config: &'a GitConfig,
    batch_updates: bool,
    oid_cache: RefCell<HashMap<String, ObjectId>>,
    object_type_cache: RefCell<HashMap<ObjectId, ObjectType>>,
}

struct UpdateRefStdinWriteRequest<'a> {
    /// The ref the write lands on (after dereferencing under `deref`).
    name: String,
    /// The ref the user typed; used in `cannot lock ref '<requested>'` so an
    /// indirect (symref) update reports the symref, not its dangling target.
    requested: String,
    new_oid: ObjectId,
    expected_oid: Option<&'a ObjectId>,
}

#[derive(Clone)]
struct UpdateRefStdinStagedWrite {
    name: String,
    requested: String,
    new_oid: ObjectId,
    expected_oid: Option<ObjectId>,
}

#[derive(Clone)]
struct UpdateRefStdinStagedDelete {
    name: String,
    requested: String,
    expected_oid: Option<ObjectId>,
}

#[derive(Clone)]
struct UpdateRefStdinStagedVerify {
    name: String,
    requested: String,
    expected_oid: ObjectId,
}

#[derive(Clone)]
struct UpdateRefStdinStagedSymrefUpdate {
    requested: String,
    name: String,
    target: String,
    expected: Option<UpdateRefStdinSymrefExpected>,
}

#[derive(Clone)]
struct UpdateRefStdinStagedSymrefCreate {
    name: String,
    target: String,
}

#[derive(Clone)]
struct UpdateRefStdinStagedSymrefDelete {
    name: String,
    expected: Option<String>,
}

#[derive(Clone)]
struct UpdateRefStdinStagedSymrefVerify {
    name: String,
    expected: Option<String>,
}

#[derive(Clone)]
enum UpdateRefStdinStagedChange {
    Write(UpdateRefStdinStagedWrite),
    Delete(UpdateRefStdinStagedDelete),
    Verify(UpdateRefStdinStagedVerify),
    SymrefCreate(UpdateRefStdinStagedSymrefCreate),
    SymrefUpdate(UpdateRefStdinStagedSymrefUpdate),
    SymrefDelete(UpdateRefStdinStagedSymrefDelete),
    SymrefVerify(UpdateRefStdinStagedSymrefVerify),
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
    RefStdinCommand {
        prefix: "update",
        args: 3,
    },
    RefStdinCommand {
        prefix: "create",
        args: 2,
    },
    RefStdinCommand {
        prefix: "delete",
        args: 2,
    },
    RefStdinCommand {
        prefix: "verify",
        args: 2,
    },
    RefStdinCommand {
        prefix: "symref-update",
        args: 4,
    },
    RefStdinCommand {
        prefix: "symref-create",
        args: 2,
    },
    RefStdinCommand {
        prefix: "symref-delete",
        args: 2,
    },
    RefStdinCommand {
        prefix: "symref-verify",
        args: 2,
    },
    RefStdinCommand {
        prefix: "option",
        args: 1,
    },
    RefStdinCommand {
        prefix: "start",
        args: 0,
    },
    RefStdinCommand {
        prefix: "prepare",
        args: 0,
    },
    RefStdinCommand {
        prefix: "abort",
        args: 0,
    },
    RefStdinCommand {
        prefix: "commit",
        args: 0,
    },
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
    let mut reader = crate::commands::stdin_stream::StdinRecordReader::new(stdin.lock(), b'\n');
    let mut stdout = io::stdout().lock();
    while let Some(mut line) = reader.read_record()? {
        crate::commands::stdin_stream::strip_trailing_cr(&mut line);
        let result =
            update_ref_stdin_line(&context, &mut deref, &mut transaction, &mut stdout, &line);
        if let Err(err) = result {
            let _ = transaction.restore(context.store);
            return Err(err);
        }
        stdout.flush()?;
    }
    transaction.finish_implicit(&context)
}

fn update_ref_stdin_line(
    context: &UpdateRefStdinContext<'_>,
    deref: &mut bool,
    transaction: &mut UpdateRefStdinTransaction,
    stdout: &mut dyn Write,
    line: &[u8],
) -> Result<()> {
    use crate::commands::ref_command_stream::{ArgCursor, Terminator, classify_line};

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

        let Some((cmd, arg_start)) = match_ref_stdin_command(&first, b'\0') else {
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
        let result = dispatch_ref_stdin_command(
            context,
            &mut deref,
            &mut transaction,
            &mut stdout,
            cmd,
            cursor,
        );
        if let Err(err) = result {
            transaction.restore(context.store)?;
            return Err(err);
        }
        stdout.flush()?;
    }
    transaction.finish_implicit(context)
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
                Some(v) => resolve_stdin_oid(context, "update", &raw_name, "<new-oid>", &v)?,
                None => zero_oid(context.format)?,
            };
            let expected = match old {
                OldOid::Absent => None,
                OldOid::Zero => Some(zero_oid(context.format)?),
                OldOid::Value(v) => Some(resolve_stdin_oid(
                    context,
                    "update",
                    &raw_name,
                    "<old-oid>",
                    &v,
                )?),
            };
            let effective = transaction.effective_ref(context.store, &raw_name, *deref)?;
            let name = effective.effective.clone();
            if context.batch_updates {
                return update_ref_stdin_write_batch(
                    context,
                    UpdateRefStdinWriteRequest {
                        name,
                        requested: effective.requested,
                        new_oid,
                        expected_oid: expected.as_ref(),
                    },
                    stdout,
                );
            }
            if transaction.capture(context.store, &effective.requested, &name)? {
                return Ok(());
            }
            let requested = effective.requested.clone();
            if let Some(err) = transaction.check_batch_df_against_queued(&requested, &name) {
                return Err(err);
            }
            if transaction.is_active() {
                transaction.stage_write(UpdateRefStdinStagedWrite {
                    name,
                    requested,
                    new_oid,
                    expected_oid: expected,
                });
                return Ok(());
            }
            transaction.mark_applied();
            update_ref_stdin_write(
                context,
                UpdateRefStdinWriteRequest {
                    name,
                    requested: effective.requested,
                    new_oid,
                    expected_oid: expected.as_ref(),
                },
            )
            .map_err(|err| transaction.reshape_df_conflict(&requested, err))
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
                Some(v) => resolve_stdin_oid(context, "create", &raw_name, "<new-oid>", &v)?,
                None => zero_oid(context.format)?,
            };
            if new_oid == zero_oid(context.format)? {
                return update_ref_stdin_create_zero(&raw_name);
            }
            let effective = transaction.effective_ref(context.store, &raw_name, *deref)?;
            let name = effective.effective.clone();
            let zero = zero_oid(context.format)?;
            if context.batch_updates {
                return update_ref_stdin_write_batch(
                    context,
                    UpdateRefStdinWriteRequest {
                        name,
                        requested: effective.requested,
                        new_oid,
                        expected_oid: Some(&zero),
                    },
                    stdout,
                );
            }
            if transaction.capture(context.store, &effective.requested, &name)? {
                return Ok(());
            }
            let requested = effective.requested.clone();
            if let Some(err) = transaction.check_batch_df_against_queued(&requested, &name) {
                return Err(err);
            }
            if transaction.is_active() {
                transaction.stage_write(UpdateRefStdinStagedWrite {
                    name,
                    requested,
                    new_oid,
                    expected_oid: Some(zero),
                });
                return Ok(());
            }
            transaction.mark_applied();
            update_ref_stdin_write(
                context,
                UpdateRefStdinWriteRequest {
                    name,
                    requested: effective.requested,
                    new_oid,
                    expected_oid: Some(&zero),
                },
            )
            .map_err(|err| transaction.reshape_df_conflict(&requested, err))
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
                    let oid = resolve_stdin_oid(context, "delete", &raw_name, "<old-oid>", &v)?;
                    // git: a resolved zero <old-oid> is also rejected.
                    if oid == zero_oid(context.format)? {
                        return update_ref_stdin_delete_zero(&raw_name);
                    }
                    Some(oid)
                }
            };
            let effective = transaction.effective_ref(context.store, &raw_name, *deref)?;
            let name = effective.effective.clone();
            if context.batch_updates {
                return update_ref_delete_stdin_batch(
                    context.store,
                    context.format,
                    &effective.requested,
                    &name,
                    expected.as_ref(),
                    stdout,
                );
            }
            if transaction.capture(context.store, &effective.requested, &name)? {
                return Ok(());
            }
            if transaction.is_active() {
                transaction.stage_delete(UpdateRefStdinStagedDelete {
                    name,
                    requested: effective.requested,
                    expected_oid: expected,
                });
                return Ok(());
            }
            transaction.mark_applied();
            update_ref_delete_stdin_named(
                context.store,
                context.format,
                &effective.requested,
                &name,
                expected.as_ref(),
            )
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
                Some(v) => resolve_stdin_oid(context, "verify", &raw_name, "<old-oid>", &v)?,
                None => zero_oid(context.format)?,
            };
            let effective = transaction.effective_ref(context.store, &raw_name, *deref)?;
            let name = effective.effective.clone();
            let current = context.store.read_ref(&name)?;
            if context.batch_updates {
                return verify_update_ref_stdin_batch(
                    context.store,
                    context.format,
                    &effective.requested,
                    &name,
                    current.as_ref(),
                    &expected,
                    stdout,
                );
            }
            if transaction.is_active() {
                transaction.stage_verify(UpdateRefStdinStagedVerify {
                    name,
                    requested: effective.requested,
                    expected_oid: expected,
                });
                return Ok(());
            }
            check_update_ref_stdin_expected_named(
                context.store,
                context.format,
                &effective.requested,
                &name,
                current.as_ref(),
                &expected,
            )
        }
        "symref-create" => {
            let Some(raw_name) = cursor.parse_refname()? else {
                return update_ref_stdin_missing_ref(verb);
            };
            let Some(target) = cursor.parse_next_refname()? else {
                return update_ref_stdin_symref_update_missing_new_target_for(
                    "symref-create",
                    &raw_name,
                );
            };
            cursor.finish("symref-create", &raw_name)?;

            let name = update_ref_effective_name(context.store, &raw_name, *deref)?;
            if transaction.capture(context.store, &raw_name, &name)? {
                return Ok(());
            }
            if transaction.is_active() {
                transaction.stage_symref_create(UpdateRefStdinStagedSymrefCreate { name, target });
                return Ok(());
            }
            transaction.mark_applied();
            update_ref_stdin_symref_create(context, &name, &target)
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

            let effective = update_ref_effective_ref(context.store, &raw_name, *deref)?;
            let name = effective.effective.clone();
            if context.batch_updates {
                return update_ref_stdin_symref_update_batch(
                    context,
                    &effective.requested,
                    &name,
                    &target,
                    expected,
                    stdout,
                );
            }
            if transaction.capture(context.store, &effective.requested, &name)? {
                return Ok(());
            }
            if transaction.is_active() {
                transaction.stage_symref_update(UpdateRefStdinStagedSymrefUpdate {
                    requested: effective.requested,
                    name,
                    target,
                    expected,
                });
                return Ok(());
            }
            transaction.mark_applied();
            update_ref_stdin_symref_update(context, &effective.requested, &name, &target, expected)
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
            if transaction.is_active() {
                transaction.stage_symref_verify(UpdateRefStdinStagedSymrefVerify {
                    name: raw_name,
                    expected,
                });
                return Ok(());
            }
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
            if transaction.capture(context.store, &raw_name, &raw_name)? {
                return Ok(());
            }
            if transaction.is_active() {
                transaction.stage_symref_delete(UpdateRefStdinStagedSymrefDelete {
                    name: raw_name,
                    expected,
                });
                return Ok(());
            }
            transaction.mark_applied();
            update_ref_stdin_symref_delete(context.store, &raw_name, expected.as_deref())
        }
        "start" => {
            cursor.finish("start", "")?;
            transaction.start(stdout)
        }
        "prepare" => {
            cursor.finish("prepare", "")?;
            transaction.prepare(context, context.git_dir, context.store, stdout)
        }
        "commit" => {
            cursor.finish("commit", "")?;
            transaction.commit(context, stdout)
        }
        "abort" => {
            cursor.finish("abort", "")?;
            transaction.abort(context, stdout)
        }
        _ => update_ref_stdin_bad_command(verb),
    }
}

/// Pull `(new_ref, existing_ref)` out of the backend's D/F-conflict message
/// `cannot lock ref '<new>': '<existing>' exists; cannot create '<new>'`.
fn parse_df_conflict_message(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix("cannot lock ref '")?;
    let (new_ref, rest) = rest.split_once("': '")?;
    let (existing_ref, _) = rest.split_once("' exists; cannot create '")?;
    Some((new_ref.to_string(), existing_ref.to_string()))
}

fn parse_non_empty_ref_directory_message(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix("cannot lock ref '")?;
    let (new_ref, rest) = rest.split_once("': there is a non-empty directory '")?;
    let (path, suffix) = rest.split_once("' blocking reference '")?;
    suffix
        .strip_suffix('\'')
        .filter(|blocked| *blocked == new_ref)?;
    Some((new_ref.to_string(), path.to_string()))
}

struct UpdateRefStdinTransaction {
    active: bool,
    explicit: bool,
    prepared: bool,
    closed: bool,
    applied: bool,
    originals: BTreeMap<String, Option<RefTarget>>,
    ref_snapshot: Option<sley_refs::RefReadSnapshot>,
    duplicate: Option<String>,
    duplicate_message: Option<String>,
    locks: Vec<PathBuf>,
    staged: Vec<UpdateRefStdinStagedChange>,
}

impl Default for UpdateRefStdinTransaction {
    fn default() -> Self {
        Self {
            active: true,
            explicit: false,
            prepared: false,
            closed: false,
            applied: false,
            originals: BTreeMap::new(),
            ref_snapshot: None,
            duplicate: None,
            duplicate_message: None,
            locks: Vec::new(),
            staged: Vec::new(),
        }
    }
}

impl UpdateRefStdinTransaction {
    /// Was `name` newly created within this batch (absent at batch start)? Used
    /// to tell git's two D/F-conflict messages apart: a conflict against a
    /// *pre-existing* ref is `'<dir>' exists; cannot create '<ref>'`, but a
    /// conflict against another ref *queued in the same batch* is
    /// `cannot process '<dir>' and '<ref>' at the same time`
    /// (git's refs_verify_refnames_available distinguishing existing refs from
    /// the `extras` set).
    fn is_batch_create(&self, name: &str) -> bool {
        matches!(self.originals.get(name), Some(None))
    }

    /// Detect a D/F conflict between `name` (a ref about to be created/updated)
    /// and a ref *already touched in this batch* whose on-disk path no longer
    /// collides — e.g. `delete foo/bar` then `create foo`, where the delete has
    /// already pruned `foo/`. git verifies all batch refnames against each other
    /// up front (`extras`), so it rejects this even though the loose write would
    /// now succeed. Reports git's message: the same-batch name yields
    /// `cannot process`, a pre-existing-then-deleted name yields
    /// `'<other>' exists; cannot create '<name>'` (the delete's old value still
    /// existed at batch start). Returns the failure if a conflict exists.
    fn check_batch_df_against_queued(&self, requested: &str, name: &str) -> Option<GitError> {
        let mut prefix = name;
        while let Some((parent, _)) = prefix.rsplit_once('/') {
            if self.originals.contains_key(parent) {
                return Some(self.batch_df_conflict_error(requested, name, parent));
            }
            prefix = parent;
        }
        let child_prefix = format!("{name}/");
        if let Some(other) = self
            .originals
            .range(child_prefix.clone()..)
            .map(|(other, _)| other.as_str())
            .next()
            .filter(|other| other.starts_with(&child_prefix))
        {
            return Some(self.batch_df_conflict_error(requested, name, other));
        }
        None
    }

    fn batch_df_conflict_error(&self, requested: &str, name: &str, other: &str) -> GitError {
        // Order parent (shorter) before child to match git's sorted output.
        let (parent, child) = if other.len() <= name.len() {
            (other, name)
        } else {
            (name, other)
        };
        if self.is_batch_create(other) {
            eprintln!("fatal: cannot process '{parent}' and '{child}' at the same time");
        } else {
            // `other` existed at batch start (a delete of a real ref): git
            // reports it as an existing-ref conflict against the new name.
            // The lock-ref prefix names the *requested* ref (the symref for
            // an indirect update), while `cannot create` names the effective
            // target.
            eprintln!(
                "fatal: cannot lock ref '{requested}': '{other}' exists; cannot create '{name}'"
            );
        }
        GitError::Exit(128)
    }

    fn effective_ref(
        &mut self,
        store: &FileRefStore,
        name: &str,
        deref: bool,
    ) -> Result<EffectiveRefName> {
        let requested = name.to_string();
        if !deref {
            return Ok(EffectiveRefName {
                effective: requested.clone(),
                requested,
            });
        }
        let mut current = requested.clone();
        for _ in 0..16 {
            match self.read_ref_for_deref(store, &current) {
                Ok(Some(RefTarget::Symbolic(target))) => current = target,
                Ok(_) => break,
                Err(GitError::InvalidPath(_)) => {
                    if sley_refs::validate_ref_name_for_update(&current).is_ok() {
                        break;
                    }
                    return update_ref_stdin_invalid_ref_format(name);
                }
                Err(GitError::InvalidFormat(message))
                    if message.starts_with(&format!("reference {current} ")) =>
                {
                    eprintln!(
                        "fatal: cannot lock ref '{requested}': unable to resolve reference '{current}': reference broken"
                    );
                    return Err(GitError::Exit(128));
                }
                Err(GitError::NotFound(sley::NotFoundKind::BrokenReference { name, .. }))
                    if name == current =>
                {
                    eprintln!(
                        "fatal: cannot lock ref '{requested}': unable to resolve reference '{current}': reference broken"
                    );
                    return Err(GitError::Exit(128));
                }
                Err(err) => return Err(err),
            }
        }
        if sley_refs::validate_ref_name_for_update(&current).is_err() {
            return update_ref_stdin_invalid_ref_format(name);
        }
        Ok(EffectiveRefName {
            requested,
            effective: current,
        })
    }

    fn read_ref_for_deref(
        &mut self,
        store: &FileRefStore,
        name: &str,
    ) -> Result<Option<RefTarget>> {
        if name == "HEAD" || !name.starts_with("refs/") {
            return store.read_ref(name);
        }
        if self.ref_snapshot.is_none() {
            self.ref_snapshot = Some(store.read_snapshot_for_update()?);
        }
        match self
            .ref_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.get(name))
        {
            Some(sley_refs::RefReadSnapshotValue::Target(target)) => Ok(Some(target.clone())),
            Some(sley_refs::RefReadSnapshotValue::Broken) => {
                Err(GitError::broken_reference(name, ""))
            }
            None => Ok(None),
        }
    }

    /// Reshape a backend D/F-conflict error into the git-shaped `fatal:` exit-128
    /// failure. git distinguishes two cases (refs_verify_refnames_available):
    ///
    ///   * the conflicting ref is *also queued in this batch* (an `extras`
    ///     conflict) — `cannot process '<parent>' and '<child>' at the same
    ///     time`, parent (shorter path) first to match git's sorted order;
    ///   * the conflicting ref *pre-exists* (e.g. delete-short + add-long) —
    ///     keep git's `'<dir>' exists; cannot create '<ref>'` text but surface it
    ///     as a `fatal:` / exit-128 die rather than the generic `transaction
    ///     failed` (exit 1) the backend's `Transaction` error renders as.
    ///
    /// Returns the original error if the message is not a D/F conflict.
    /// `requested` is the ref the user typed (the symref for an indirect update);
    /// it replaces the effective ref in the `cannot lock ref '<...>'` prefix so
    /// an indirect create reports the symref, matching git.
    fn reshape_df_conflict(&self, requested: &str, err: GitError) -> GitError {
        let GitError::Transaction(message) = &err else {
            return err;
        };
        if let Some((new_ref, path)) = parse_non_empty_ref_directory_message(message) {
            eprintln!(
                "fatal: cannot lock ref '{requested}': there is a non-empty directory '{path}' blocking reference '{new_ref}'"
            );
            return GitError::Exit(128);
        }
        // Backend message: "cannot lock ref '<new>': '<existing>' exists; cannot create '<new>'".
        let Some((new_ref, existing_ref)) = parse_df_conflict_message(message) else {
            return err;
        };
        if self.is_batch_create(&existing_ref) {
            // Parent (shorter path) first, child second — matching git's sorted order.
            let (parent, child) = if existing_ref.len() <= new_ref.len() {
                (existing_ref, new_ref)
            } else {
                (new_ref, existing_ref)
            };
            eprintln!("fatal: cannot process '{parent}' and '{child}' at the same time");
        } else {
            eprintln!(
                "fatal: cannot lock ref '{requested}': '{existing_ref}' exists; cannot create '{new_ref}'"
            );
        }
        GitError::Exit(128)
    }

    fn capture(&mut self, store: &FileRefStore, requested: &str, name: &str) -> Result<bool> {
        if self.active {
            if self.explicit {
                if !self.originals.contains_key(name) {
                    let original = self.original_ref(store, name)?;
                    self.originals.insert(name.to_string(), original);
                }
                return Ok(false);
            }
            if self.originals.contains_key(name) {
                if self.duplicate.is_none() {
                    self.duplicate = Some(name.to_string());
                    if requested != name {
                        self.duplicate_message = Some(format!(
                            "multiple updates for '{name}' (including one via symref '{requested}') are not allowed"
                        ));
                    }
                }
                return Ok(true);
            }
            if requested == "HEAD"
                && name == "HEAD"
                && let Some(RefTarget::Symbolic(head_target)) = store.read_ref("HEAD")?
                && self.originals.contains_key(&head_target)
            {
                if self.duplicate.is_none() {
                    self.duplicate = Some("HEAD".to_string());
                    self.duplicate_message = Some(format!(
                        "multiple updates for 'HEAD' (including one via its referent '{head_target}') are not allowed"
                    ));
                }
                return Ok(true);
            }
            let original = self.original_ref(store, name)?;
            self.originals.insert(name.to_string(), original);
        }
        Ok(false)
    }

    fn original_ref(&mut self, store: &FileRefStore, name: &str) -> Result<Option<RefTarget>> {
        self.read_ref_for_deref(store, name)
    }

    fn is_prepared(&self) -> bool {
        self.prepared
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    fn is_implicit(&self) -> bool {
        self.active && !self.explicit && !self.prepared
    }

    fn is_explicit(&self) -> bool {
        self.active && self.explicit
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn mark_applied(&mut self) {
        self.applied = true;
    }

    fn stage_write(&mut self, write: UpdateRefStdinStagedWrite) {
        self.staged.push(UpdateRefStdinStagedChange::Write(write));
    }

    fn stage_delete(&mut self, delete: UpdateRefStdinStagedDelete) {
        self.staged.push(UpdateRefStdinStagedChange::Delete(delete));
    }

    fn stage_verify(&mut self, verify: UpdateRefStdinStagedVerify) {
        self.staged.push(UpdateRefStdinStagedChange::Verify(verify));
    }

    fn stage_symref_update(&mut self, update: UpdateRefStdinStagedSymrefUpdate) {
        self.staged
            .push(UpdateRefStdinStagedChange::SymrefUpdate(update));
    }

    fn stage_symref_create(&mut self, create: UpdateRefStdinStagedSymrefCreate) {
        self.staged
            .push(UpdateRefStdinStagedChange::SymrefCreate(create));
    }

    fn stage_symref_delete(&mut self, delete: UpdateRefStdinStagedSymrefDelete) {
        self.staged
            .push(UpdateRefStdinStagedChange::SymrefDelete(delete));
    }

    fn stage_symref_verify(&mut self, verify: UpdateRefStdinStagedSymrefVerify) {
        self.staged
            .push(UpdateRefStdinStagedChange::SymrefVerify(verify));
    }

    fn start(&mut self, stdout: &mut dyn Write) -> Result<()> {
        if self.explicit {
            return update_ref_stdin_restart_transaction();
        }
        self.active = true;
        self.explicit = true;
        self.prepared = false;
        self.closed = false;
        self.applied = false;
        self.staged.clear();
        self.ref_snapshot = None;
        writeln!(stdout, "start: ok")?;
        Ok(())
    }

    fn prepare(
        &mut self,
        context: &UpdateRefStdinContext<'_>,
        git_dir: &Path,
        store: &FileRefStore,
        stdout: &mut dyn Write,
    ) -> Result<()> {
        if let Some(name) = self.duplicate.clone() {
            let message = self
                .duplicate_message
                .clone()
                .or_else(|| self.infer_duplicate_message(store, &name).ok().flatten());
            self.restore(store)?;
            update_ref_stdin_duplicate_failure("prepare", &name, message.as_deref());
            return Err(GitError::Exit(128));
        }
        if self.explicit
            && let Some((name, message)) = self.staged_duplicate()
        {
            self.restore(store)?;
            update_ref_stdin_duplicate_failure("prepare", &name, message.as_deref());
            return Err(GitError::Exit(128));
        }
        if self.explicit {
            self.run_explicit_hook(context, RefTransactionPhase::Preparing)?;
            self.run_explicit_files_symref_delete_abort_hook(context)?;
        }
        self.acquire_locks(git_dir)?;
        if self.explicit {
            self.run_explicit_hook(context, RefTransactionPhase::Prepared)?;
        }
        self.prepared = true;
        writeln!(stdout, "prepare: ok")?;
        Ok(())
    }

    fn commit(
        &mut self,
        context: &UpdateRefStdinContext<'_>,
        stdout: &mut dyn Write,
    ) -> Result<()> {
        if let Some(name) = self.duplicate.clone() {
            let message = self.duplicate_message.clone().or_else(|| {
                self.infer_duplicate_message(context.store, &name)
                    .ok()
                    .flatten()
            });
            self.restore(context.store)?;
            update_ref_stdin_duplicate_failure("commit", &name, message.as_deref());
            return Err(GitError::Exit(128));
        }
        if self.explicit
            && let Some((name, message)) = self.staged_duplicate()
        {
            self.restore(context.store)?;
            update_ref_stdin_duplicate_failure("commit", &name, message.as_deref());
            return Err(GitError::Exit(128));
        }
        let staged = mem::take(&mut self.staged);
        if !staged.is_empty() {
            if self.explicit {
                if !self.prepared {
                    self.run_explicit_hook_for_staged(
                        context,
                        RefTransactionPhase::Preparing,
                        &staged,
                    )?;
                    self.acquire_locks(context.git_dir)?;
                    self.run_explicit_hook_for_staged(
                        context,
                        RefTransactionPhase::Prepared,
                        &staged,
                    )?;
                }
                self.release_locks();
                update_ref_stdin_commit_staged(context, staged.clone(), false)?;
                self.run_explicit_hook_for_staged(
                    context,
                    RefTransactionPhase::Committed,
                    &staged,
                )?;
            } else {
                self.release_locks();
                update_ref_stdin_commit_staged(context, staged, true)?;
            }
        }
        self.active = false;
        self.explicit = false;
        self.prepared = false;
        self.closed = true;
        self.release_locks();
        self.originals.clear();
        self.ref_snapshot = None;
        self.duplicate_message = None;
        self.duplicate = None;
        writeln!(stdout, "commit: ok")?;
        Ok(())
    }

    fn finish_implicit(&mut self, context: &UpdateRefStdinContext<'_>) -> Result<()> {
        if self.explicit || self.prepared {
            return self.restore(context.store);
        }
        if let Some(name) = self.duplicate.clone() {
            let message = self.duplicate_message.clone().or_else(|| {
                self.infer_duplicate_message(context.store, &name)
                    .ok()
                    .flatten()
            });
            self.restore(context.store)?;
            update_ref_stdin_duplicate_failure("", &name, message.as_deref());
            return Err(GitError::Exit(128));
        }
        if !self.staged.is_empty() {
            let staged = mem::take(&mut self.staged);
            update_ref_stdin_commit_staged(context, staged, true)?;
        }
        self.active = false;
        self.prepared = false;
        self.originals.clear();
        self.ref_snapshot = None;
        self.duplicate_message = None;
        self.closed = false;
        self.applied = false;
        Ok(())
    }

    fn abort(&mut self, context: &UpdateRefStdinContext<'_>, stdout: &mut dyn Write) -> Result<()> {
        if self.explicit {
            self.run_explicit_hook(context, RefTransactionPhase::Aborted)?;
        }
        self.restore(context.store)?;
        writeln!(stdout, "abort: ok")?;
        Ok(())
    }

    fn run_explicit_hook(
        &self,
        context: &UpdateRefStdinContext<'_>,
        phase: RefTransactionPhase,
    ) -> Result<()> {
        self.run_explicit_hook_for_staged(context, phase, &self.staged)
    }

    fn run_explicit_hook_for_staged(
        &self,
        context: &UpdateRefStdinContext<'_>,
        phase: RefTransactionPhase,
        staged: &[UpdateRefStdinStagedChange],
    ) -> Result<()> {
        let updates = update_ref_stdin_hook_updates(context, staged)?;
        if updates.is_empty() {
            return Ok(());
        }
        let hook = ReferenceTransactionHookRunner::new(context.git_dir);
        if hook.run(phase, &updates)?
            && matches!(
                phase,
                RefTransactionPhase::Preparing | RefTransactionPhase::Prepared
            )
        {
            return Err(GitError::Transaction(format!(
                "in '{}' phase, update aborted by the reference-transaction hook",
                phase.as_str()
            )));
        }
        Ok(())
    }

    fn run_explicit_files_symref_delete_abort_hook(
        &self,
        context: &UpdateRefStdinContext<'_>,
    ) -> Result<()> {
        if context.store.uses_reftable()? {
            return Ok(());
        }
        let zero = zero_oid(context.format)?.to_string();
        let updates = self
            .staged
            .iter()
            .filter_map(|change| match change {
                UpdateRefStdinStagedChange::SymrefDelete(delete) => {
                    Some(RefTransactionHookUpdate {
                        old_value: zero.clone(),
                        new_value: zero.clone(),
                        refname: delete.name.clone(),
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if updates.is_empty() {
            return Ok(());
        }
        let hook = ReferenceTransactionHookRunner::new(context.git_dir);
        let _ = hook.run(RefTransactionPhase::Aborted, &updates)?;
        Ok(())
    }

    fn restore(&mut self, store: &FileRefStore) -> Result<()> {
        self.release_locks();
        if self.active && self.applied {
            for (name, original) in mem::take(&mut self.originals) {
                update_ref_stdin_restore_ref(store, &name, original)?;
            }
            self.active = false;
        }
        if !self.applied {
            self.originals.clear();
        }
        self.ref_snapshot = None;
        self.staged.clear();
        self.explicit = false;
        self.prepared = false;
        self.closed = true;
        self.duplicate = None;
        self.duplicate_message = None;
        self.applied = false;
        Ok(())
    }

    fn infer_duplicate_message(&self, store: &FileRefStore, name: &str) -> Result<Option<String>> {
        if name == "HEAD"
            && let Some(RefTarget::Symbolic(head_target)) = store.read_ref("HEAD")?
            && self.originals.contains_key(&head_target)
        {
            return Ok(Some(format!(
                "multiple updates for 'HEAD' (including one via its referent '{head_target}') are not allowed"
            )));
        }
        for reference in store.list_refs()? {
            if let RefTarget::Symbolic(target) = reference.target
                && target == name
            {
                return Ok(Some(format!(
                    "multiple updates for '{name}' (including one via symref '{}') are not allowed",
                    reference.name
                )));
            }
        }
        Ok(None)
    }

    fn staged_duplicate(&self) -> Option<(String, Option<String>)> {
        let mut seen = BTreeMap::<&str, &str>::new();
        for change in &self.staged {
            let Some((name, requested)) = staged_ref_change_name(change) else {
                continue;
            };
            if let Some(previous_requested) = seen.insert(name, requested) {
                let message = if requested != name {
                    Some(format!(
                        "multiple updates for '{name}' (including one via symref '{requested}') are not allowed"
                    ))
                } else if previous_requested != name {
                    Some(format!(
                        "multiple updates for '{name}' (including one via symref '{previous_requested}') are not allowed"
                    ))
                } else {
                    None
                };
                return Some((name.to_string(), message));
            }
        }
        None
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
            let _ = fs::remove_file(&lock);
            prune_empty_ref_lock_dirs(&lock);
        }
    }
}

fn prune_empty_ref_lock_dirs(lock_path: &Path) {
    let Some(mut dir) = lock_path.parent() else {
        return;
    };
    while dir.file_name().is_some_and(|name| name != "refs") {
        if fs::remove_dir(dir).is_err() {
            break;
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent;
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

fn update_ref_stdin_invalid_ref_format(name: &str) -> Result<EffectiveRefName> {
    eprintln!("fatal: invalid ref format: {name}");
    Err(GitError::Exit(128))
}

fn update_ref_stdin_duplicate_failure(phase: &str, name: &str, message: Option<&str>) {
    match (phase.is_empty(), message) {
        (true, Some(message)) => eprintln!("fatal: {message}"),
        (false, Some(message)) => eprintln!("fatal: {phase}: {message}"),
        (true, None) => eprintln!("fatal: multiple updates for ref '{name}' not allowed"),
        (false, None) => {
            eprintln!("fatal: {phase}: multiple updates for ref '{name}' not allowed")
        }
    }
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

/// The current target of a staged ref, preferring the pre-read `list_refs`
/// snapshot but falling back to a direct read. `list_refs()` does not enumerate
/// root-level symrefs (e.g. a `TESTSYMREF` created by `symbolic-ref`), so a
/// `--no-deref` update/delete of such a ref would otherwise see it as missing
/// and fail the old-value check with `unable to resolve reference`.
fn update_ref_stdin_current_target(
    context: &UpdateRefStdinContext<'_>,
    current_refs: &HashMap<String, RefTarget>,
    name: &str,
) -> Result<Option<RefTarget>> {
    match current_refs.get(name).cloned() {
        Some(target) => Ok(Some(target)),
        None => context.store.read_ref(name),
    }
}

fn update_ref_stdin_hook_updates(
    context: &UpdateRefStdinContext<'_>,
    staged: &[UpdateRefStdinStagedChange],
) -> Result<Vec<RefTransactionHookUpdate>> {
    let zero = zero_oid(context.format)?.to_string();
    let mut updates = Vec::new();
    for change in staged {
        match change {
            UpdateRefStdinStagedChange::Write(write) => {
                updates.push(RefTransactionHookUpdate {
                    old_value: write
                        .expected_oid
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| zero.clone()),
                    new_value: write.new_oid.to_string(),
                    refname: write.requested.clone(),
                });
            }
            UpdateRefStdinStagedChange::Delete(delete) => {
                updates.push(RefTransactionHookUpdate {
                    old_value: delete
                        .expected_oid
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| zero.clone()),
                    new_value: zero.clone(),
                    refname: delete.requested.clone(),
                });
            }
            UpdateRefStdinStagedChange::Verify(verify) => {
                updates.push(RefTransactionHookUpdate {
                    old_value: verify.expected_oid.to_string(),
                    new_value: zero.clone(),
                    refname: verify.requested.clone(),
                });
            }
            UpdateRefStdinStagedChange::SymrefCreate(create) => {
                updates.push(RefTransactionHookUpdate {
                    old_value: zero.clone(),
                    new_value: hook_symref_value(&create.target),
                    refname: create.name.clone(),
                });
            }
            UpdateRefStdinStagedChange::SymrefUpdate(update) => {
                updates.push(RefTransactionHookUpdate {
                    old_value: update_ref_stdin_symref_expected_hook_value(
                        update.expected.as_ref(),
                        &zero,
                    ),
                    new_value: hook_symref_value(&update.target),
                    refname: update.requested.clone(),
                });
            }
            UpdateRefStdinStagedChange::SymrefDelete(delete) => {
                updates.push(RefTransactionHookUpdate {
                    old_value: delete
                        .expected
                        .as_deref()
                        .map(hook_symref_value)
                        .unwrap_or_else(|| zero.clone()),
                    new_value: zero.clone(),
                    refname: delete.name.clone(),
                });
            }
            UpdateRefStdinStagedChange::SymrefVerify(verify) => {
                updates.push(RefTransactionHookUpdate {
                    old_value: verify
                        .expected
                        .as_deref()
                        .map(hook_symref_value)
                        .unwrap_or_else(|| zero.clone()),
                    new_value: zero.clone(),
                    refname: verify.name.clone(),
                });
            }
        }
    }
    Ok(updates)
}

fn staged_ref_change_name(change: &UpdateRefStdinStagedChange) -> Option<(&str, &str)> {
    match change {
        UpdateRefStdinStagedChange::Write(write) => Some((&write.name, &write.requested)),
        UpdateRefStdinStagedChange::Delete(delete) => Some((&delete.name, &delete.requested)),
        UpdateRefStdinStagedChange::Verify(verify) => Some((&verify.name, &verify.requested)),
        UpdateRefStdinStagedChange::SymrefCreate(create) => Some((&create.name, &create.name)),
        UpdateRefStdinStagedChange::SymrefUpdate(update) => Some((&update.name, &update.requested)),
        UpdateRefStdinStagedChange::SymrefDelete(delete) => Some((&delete.name, &delete.name)),
        UpdateRefStdinStagedChange::SymrefVerify(verify) => Some((&verify.name, &verify.name)),
    }
}

fn hook_symref_value(target: &str) -> String {
    format!("ref:{target}")
}

fn update_ref_stdin_symref_expected_hook_value(
    expected: Option<&UpdateRefStdinSymrefExpected>,
    zero: &str,
) -> String {
    match expected {
        Some(UpdateRefStdinSymrefExpected::Target(target)) => hook_symref_value(target),
        Some(UpdateRefStdinSymrefExpected::Oid(oid)) => oid.to_string(),
        None => zero.to_string(),
    }
}

fn update_ref_stdin_commit_staged(
    context: &UpdateRefStdinContext<'_>,
    staged: Vec<UpdateRefStdinStagedChange>,
    run_hooks: bool,
) -> Result<()> {
    let warn_symlink_refs = context.store.prefers_symlink_refs()
        && staged.iter().any(|change| {
            matches!(
                change,
                UpdateRefStdinStagedChange::SymrefCreate(_)
                    | UpdateRefStdinStagedChange::SymrefUpdate(_)
            )
        });
    let zero = zero_oid(context.format)?;
    let hook = ReferenceTransactionHookRunner::new(context.git_dir);
    let mut tx = context.store.transaction();
    if run_hooks {
        tx = tx.with_hook(&hook);
    }
    let mut requested_by_name = BTreeMap::new();
    let current_refs = context
        .store
        .list_refs()?
        .into_iter()
        .map(|reference| (reference.name, reference.target))
        .collect::<HashMap<_, _>>();
    for change in staged {
        match change {
            UpdateRefStdinStagedChange::Write(write) => {
                requested_by_name.insert(write.name.clone(), write.requested.clone());
                let current = update_ref_stdin_current_target(context, &current_refs, &write.name)?;
                if let Some(expected_oid) = write.expected_oid.as_ref() {
                    check_update_ref_stdin_expected_named(
                        context.store,
                        context.format,
                        &write.requested,
                        &write.name,
                        current.as_ref(),
                        expected_oid,
                    )?;
                }
                if write.new_oid == zero {
                    if current.is_some() {
                        tx.delete_with_precondition(
                            write.name,
                            sley_refs::RefDeletePrecondition::Any,
                            None,
                        );
                    }
                    continue;
                }
                check_update_ref_new_value_cached(
                    context,
                    &write.name,
                    &write.requested,
                    &write.new_oid,
                )
                .map_err(|reason| {
                    eprintln!("fatal: {reason}");
                    GitError::Exit(128)
                })?;
                let old_oid = match current {
                    Some(RefTarget::Direct(oid)) => oid,
                    _ => zero,
                };
                let reflog = update_ref_should_write_reflog(
                    context.git_dir,
                    &write.name,
                    context.create_reflog,
                )?
                .then(|| ReflogEntry {
                    old_oid,
                    new_oid: write.new_oid,
                    committer: ref_reflog_committer(context.config),
                    message: context.message.clone(),
                });
                tx.update(RefUpdate {
                    name: write.name,
                    expected: None,
                    new: RefTarget::Direct(write.new_oid),
                    reflog,
                });
            }
            UpdateRefStdinStagedChange::Delete(delete) => {
                requested_by_name.insert(delete.name.clone(), delete.requested.clone());
                let current =
                    update_ref_stdin_current_target(context, &current_refs, &delete.name)?;
                if let Some(expected_oid) = delete.expected_oid.as_ref() {
                    check_update_ref_stdin_expected_named(
                        context.store,
                        context.format,
                        &delete.requested,
                        &delete.name,
                        current.as_ref(),
                        expected_oid,
                    )?;
                }
                if current.is_some() {
                    tx.delete_with_precondition(
                        delete.name,
                        sley_refs::RefDeletePrecondition::Any,
                        None,
                    );
                }
            }
            UpdateRefStdinStagedChange::Verify(verify) => {
                requested_by_name.insert(verify.name.clone(), verify.requested.clone());
                let current = context.store.read_ref(&verify.name)?;
                check_update_ref_stdin_expected_named(
                    context.store,
                    context.format,
                    &verify.requested,
                    &verify.name,
                    current.as_ref(),
                    &verify.expected_oid,
                )?;
            }
            UpdateRefStdinStagedChange::SymrefCreate(create) => {
                requested_by_name.insert(create.name.clone(), create.name.clone());
                sley_refs::validate_ref_name_for_update(&create.name)?;
                if let Some(current) = context.store.read_ref(&create.name)? {
                    return match current {
                        RefTarget::Symbolic(_) => {
                            update_ref_stdin_symref_exists(&create.name, true)
                        }
                        RefTarget::Direct(_) => update_ref_stdin_symref_exists(&create.name, false),
                    };
                }
                let reflog = update_ref_stdin_symref_reflog(context, &create.name, &create.target)?;
                tx.update(RefUpdate {
                    name: create.name,
                    expected: None,
                    new: RefTarget::Symbolic(create.target),
                    reflog,
                });
            }
            UpdateRefStdinStagedChange::SymrefUpdate(update) => {
                requested_by_name.insert(update.name.clone(), update.requested.clone());
                sley_refs::validate_ref_name_for_update(&update.name)?;
                let current =
                    update_ref_stdin_current_target(context, &current_refs, &update.name)?;
                check_update_ref_stdin_symref_expected(
                    context,
                    &update.requested,
                    &update.name,
                    current.as_ref(),
                    update.expected.as_ref(),
                )?;
                let reflog = update_ref_stdin_symref_reflog(context, &update.name, &update.target)?;
                tx.update(RefUpdate {
                    name: update.name.clone(),
                    expected: None,
                    new: RefTarget::Symbolic(update.target.clone()),
                    reflog: reflog.clone(),
                });
                if update.requested != update.name
                    && let Some(reflog) = reflog
                {
                    context.store.append_reflog(&update.requested, &reflog)?;
                }
            }
            UpdateRefStdinStagedChange::SymrefDelete(delete) => {
                requested_by_name.insert(delete.name.clone(), delete.name.clone());
                sley_refs::validate_ref_name_for_update(&delete.name)?;
                let current =
                    update_ref_stdin_current_target(context, &current_refs, &delete.name)?;
                if let Some(expected) = delete.expected.as_deref() {
                    update_ref_stdin_symref_verify_current(
                        &delete.name,
                        current.as_ref(),
                        Some(expected),
                    )?;
                }
                match current {
                    Some(RefTarget::Symbolic(target)) => {
                        tx.delete_with_precondition(
                            delete.name,
                            sley_refs::RefDeletePrecondition::Immediate(RefTarget::Symbolic(
                                target,
                            )),
                            None,
                        );
                    }
                    Some(RefTarget::Direct(_)) if delete.expected.is_none() => {
                        tx.delete_with_precondition(
                            delete.name,
                            sley_refs::RefDeletePrecondition::Direct(None),
                            None,
                        );
                    }
                    Some(RefTarget::Direct(_)) => {}
                    None => {}
                }
            }
            UpdateRefStdinStagedChange::SymrefVerify(verify) => {
                requested_by_name.insert(verify.name.clone(), verify.name.clone());
                sley_refs::validate_ref_name_for_update(&verify.name)?;
                let current =
                    update_ref_stdin_current_target(context, &current_refs, &verify.name)?;
                update_ref_stdin_symref_verify_current(
                    &verify.name,
                    current.as_ref(),
                    verify.expected.as_deref(),
                )?;
            }
        }
    }
    let result = match tx.commit() {
        Err(GitError::Transaction(message)) => {
            if let Some((new_ref, path)) = parse_non_empty_ref_directory_message(&message) {
                let requested = requested_by_name
                    .get(&new_ref)
                    .map(String::as_str)
                    .unwrap_or(new_ref.as_str());
                eprintln!(
                    "fatal: cannot lock ref '{requested}': there is a non-empty directory '{path}' blocking reference '{new_ref}'"
                );
            } else if let Some((new_ref, existing_ref)) = parse_df_conflict_message(&message) {
                let requested = requested_by_name
                    .get(&new_ref)
                    .map(String::as_str)
                    .unwrap_or(new_ref.as_str());
                eprintln!(
                    "fatal: cannot lock ref '{requested}': '{existing_ref}' exists; cannot create '{new_ref}'"
                );
            } else {
                eprintln!("fatal: {message}");
            }
            Err(GitError::Exit(128))
        }
        result => result,
    };
    if result.is_ok() && warn_symlink_refs {
        warn_prefer_symlink_refs_deprecated();
    }
    result
}

fn warn_prefer_symlink_refs_deprecated() {
    eprintln!("warning: 'core.preferSymlinkRefs=true' is nominated for removal.");
    eprintln!("hint: The use of symbolic links for symbolic refs is deprecated");
    eprintln!("hint: and will be removed in Git 3.0. The configuration that");
    eprintln!("hint: tells Git to use them is thus going away. You can unset");
    eprintln!("hint: it with:");
    eprintln!("hint:");
    eprintln!("hint:\tgit config unset core.preferSymlinkRefs");
    eprintln!("hint:");
    eprintln!("hint: Git will then use the textual symref format instead.");
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

fn update_ref_stdin_symref_create(
    context: &UpdateRefStdinContext<'_>,
    name: &str,
    target: &str,
) -> Result<()> {
    sley_refs::validate_ref_name_for_update(name)?;
    if let Some(current) = context.store.read_ref(name)? {
        return match current {
            RefTarget::Symbolic(_) => update_ref_stdin_symref_exists(name, true),
            RefTarget::Direct(_) => update_ref_stdin_symref_exists(name, false),
        };
    }
    let reflog = update_ref_stdin_symref_reflog(context, name, target)?;
    let mut tx = context.store.transaction();
    tx.update(RefUpdate {
        name: name.to_string(),
        expected: None,
        new: RefTarget::Symbolic(target.to_string()),
        reflog,
    });
    tx.commit()
}

#[derive(Clone)]
enum UpdateRefStdinSymrefExpected {
    Target(String),
    Oid(ObjectId),
}

fn update_ref_stdin_symref_update(
    context: &UpdateRefStdinContext<'_>,
    requested: &str,
    name: &str,
    target: &str,
    expected: Option<UpdateRefStdinSymrefExpected>,
) -> Result<()> {
    sley_refs::validate_ref_name_for_update(name)?;
    match expected {
        Some(UpdateRefStdinSymrefExpected::Target(expected)) => {
            update_ref_stdin_symref_verify(context.store, name, Some(&expected))?;
        }
        Some(UpdateRefStdinSymrefExpected::Oid(expected)) => {
            let current = context.store.read_ref(name)?;
            update_ref_stdin_symref_verify_oid(
                context.store,
                context.format,
                requested,
                name,
                current.as_ref(),
                &expected,
            )?;
        }
        None => {}
    }
    let reflog = update_ref_stdin_symref_reflog(context, name, target)?;
    let mut tx = context.store.transaction();
    tx.update(RefUpdate {
        name: name.to_string(),
        expected: None,
        new: RefTarget::Symbolic(target.to_string()),
        reflog,
    });
    tx.commit()?;
    if requested != name {
        if let Some(reflog) = update_ref_stdin_symref_reflog(context, name, target)? {
            context.store.append_reflog(requested, &reflog)?;
        }
    }
    Ok(())
}

fn update_ref_stdin_symref_update_batch(
    context: &UpdateRefStdinContext<'_>,
    requested: &str,
    name: &str,
    target: &str,
    expected: Option<UpdateRefStdinSymrefExpected>,
    stdout: &mut dyn Write,
) -> Result<()> {
    validate_ref_name(name)?;
    if let Some(UpdateRefStdinSymrefExpected::Target(expected_target)) = expected.as_ref()
        && matches!(context.store.read_ref(name)?, Some(RefTarget::Direct(_)))
    {
        let zero = zero_oid(context.format)?.to_string();
        print_update_ref_stdin_rejection(
            UpdateRefStdinRejection {
                name: name.to_string(),
                requested: requested.to_string(),
                new_value: zero.clone(),
                old_value: zero,
                stdout_reason: "expected symref but found regular ref",
                stderr_reason: format!(
                    "expected symref with target '{expected_target}': but is a regular ref"
                ),
            },
            stdout,
        )?;
        return Ok(());
    }
    update_ref_stdin_symref_update(context, requested, name, target, expected)
}

fn update_ref_stdin_symref_reflog(
    context: &UpdateRefStdinContext<'_>,
    name: &str,
    target: &str,
) -> Result<Option<ReflogEntry>> {
    if !update_ref_should_write_reflog(context.git_dir, name, context.create_reflog)? {
        return Ok(None);
    }
    let zero = zero_oid(context.format)?;
    let old_oid = resolve_ref_peeled(context.store, name)?.unwrap_or(zero);
    let new_oid = resolve_ref_peeled(context.store, target)?.unwrap_or(zero);
    Ok(Some(ReflogEntry {
        old_oid,
        new_oid,
        committer: ref_reflog_committer(context.config),
        message: context.message.clone(),
    }))
}

fn update_ref_stdin_symref_verify(
    store: &FileRefStore,
    name: &str,
    expected: Option<&str>,
) -> Result<()> {
    validate_ref_name(name)?;
    let current = store.read_ref(name)?;
    update_ref_stdin_symref_verify_current(name, current.as_ref(), expected)
}

fn update_ref_stdin_symref_verify_current(
    name: &str,
    current: Option<&RefTarget>,
    expected: Option<&str>,
) -> Result<()> {
    match (current, expected) {
        (None, None) => Ok(()),
        (None, Some(_)) => update_ref_stdin_symref_unresolved(name),
        (Some(RefTarget::Direct(_)), Some(expected)) => {
            eprintln!(
                "fatal: cannot lock ref '{name}': expected symref with target '{expected}': but is a regular ref"
            );
            Err(GitError::Exit(128))
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

fn check_update_ref_stdin_symref_expected(
    context: &UpdateRefStdinContext<'_>,
    requested: &str,
    name: &str,
    current: Option<&RefTarget>,
    expected: Option<&UpdateRefStdinSymrefExpected>,
) -> Result<()> {
    match expected {
        Some(UpdateRefStdinSymrefExpected::Target(expected)) => {
            update_ref_stdin_symref_verify_current(name, current, Some(expected))
        }
        Some(UpdateRefStdinSymrefExpected::Oid(expected)) => update_ref_stdin_symref_verify_oid(
            context.store,
            context.format,
            requested,
            name,
            current,
            expected,
        ),
        None => Ok(()),
    }
}

fn update_ref_stdin_symref_verify_oid(
    store: &FileRefStore,
    format: ObjectFormat,
    requested: &str,
    name: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
) -> Result<()> {
    let zero = zero_oid(format)?;
    if matches!(current, Some(RefTarget::Symbolic(_))) && expected != &zero {
        eprintln!(
            "fatal: cannot lock ref '{requested}': reference is missing but expected {expected}"
        );
        return Err(GitError::Exit(128));
    }
    check_update_ref_stdin_expected_named(store, format, requested, name, current, expected)
}

fn update_ref_stdin_symref_delete(
    store: &FileRefStore,
    name: &str,
    expected: Option<&str>,
) -> Result<()> {
    validate_ref_name(name)?;
    if let Some(expected) = expected {
        update_ref_stdin_symref_verify(store, name, Some(expected))?;
    }
    match store.read_ref(name)? {
        Some(RefTarget::Symbolic(_)) => {
            let _ = store.delete_symbolic_ref(name)?;
        }
        Some(RefTarget::Direct(_)) if expected.is_none() => {
            let _ = store.delete_ref(name)?;
        }
        Some(RefTarget::Direct(_)) => {}
        None => {}
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
    /// The effective (dereferenced) ref name printed in the `rejected …` stdout
    /// line.
    name: String,
    /// The ref name the user typed, printed in the `cannot lock ref '…'` stderr
    /// line — for a dereferenced symref delete these differ (git reports the
    /// symref on stderr but its dangling target on stdout).
    requested: String,
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
        rejection.requested, rejection.stderr_reason
    );
    Ok(())
}

fn print_update_ref_stdin_case_conflict_rejection(
    context: &UpdateRefStdinContext<'_>,
    name: &str,
    new_value: String,
    old_value: String,
    stdout: &mut dyn Write,
) -> Result<()> {
    writeln!(
        stdout,
        "rejected {name} {new_value} {old_value} reference conflict due to case-insensitive filesystem"
    )?;
    let lock_path = lock_path_for_loose_ref_path(&loose_ref_path_for_ref(context.git_dir, name)?)?;
    eprintln!(
        "error: cannot lock ref '{name}': Unable to create '{}': File exists",
        lock_path.display()
    );
    Ok(())
}

fn find_case_insensitive_ref_conflict(store: &FileRefStore, name: &str) -> Result<Option<String>> {
    for reference in store.list_refs()? {
        if reference.name != name && reference.name.eq_ignore_ascii_case(name) {
            return Ok(Some(reference.name));
        }
    }
    Ok(None)
}

fn update_ref_stdin_expected_rejection(
    store: &FileRefStore,
    format: ObjectFormat,
    requested: &str,
    name: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
    new_value: String,
) -> Result<Option<UpdateRefStdinRejection>> {
    let make = |stdout_reason, stderr_reason, old_value: String| UpdateRefStdinRejection {
        name: name.to_string(),
        requested: requested.to_string(),
        new_value: new_value.clone(),
        old_value,
        stdout_reason,
        stderr_reason,
    };
    let zero = zero_oid(format)?;
    if expected == &zero {
        if current.is_some() {
            return Ok(Some(make(
                "reference already exists",
                "reference already exists".to_string(),
                zero.to_string(),
            )));
        }
        return Ok(None);
    }

    // Mirror check_update_ref_stdin_expected_named: a `--no-deref` symref is
    // resolved one chain to an OID so its dangling target reports
    // `reference is missing but expected X` (stderr) / `reference does not exist`
    // (stdout), distinct from a wholly-missing ref's `unable to resolve`.
    match current {
        Some(RefTarget::Direct(actual)) if actual == expected => Ok(None),
        Some(RefTarget::Direct(actual)) => Ok(Some(make(
            "incorrect old value provided",
            format!("is at {actual} but expected {expected}"),
            expected.to_string(),
        ))),
        Some(RefTarget::Symbolic(_)) => match resolve_ref_peeled(store, name)? {
            Some(actual) if &actual == expected => Ok(None),
            Some(actual) => Ok(Some(make(
                "incorrect old value provided",
                format!("is at {actual} but expected {expected}"),
                expected.to_string(),
            ))),
            None => Ok(Some(make(
                "reference does not exist",
                format!("reference is missing but expected {expected}"),
                expected.to_string(),
            ))),
        },
        None => Ok(Some(make(
            "reference does not exist",
            format!("unable to resolve reference '{name}'"),
            expected.to_string(),
        ))),
    }
}

fn check_update_ref_new_value(
    git_dir: &Path,
    format: ObjectFormat,
    name: &str,
    display_name: &str,
    new_oid: &ObjectId,
) -> std::result::Result<(), String> {
    if new_oid == &zero_oid(format).map_err(|err| err.to_string())? {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(new_oid).map_err(|err| match err {
        GitError::NotFound(_) => {
            format!("trying to write ref '{display_name}' with nonexistent object {new_oid}")
        }
        err => err.to_string(),
    })?;
    if update_ref_requires_commit(name) && object.object_type != ObjectType::Commit {
        return Err(format!(
            "trying to write non-commit object {new_oid} to branch '{display_name}'"
        ));
    }
    Ok(())
}

fn check_update_ref_new_value_cached(
    context: &UpdateRefStdinContext<'_>,
    name: &str,
    display_name: &str,
    new_oid: &ObjectId,
) -> std::result::Result<(), String> {
    if new_oid == &zero_oid(context.format).map_err(|err| err.to_string())? {
        return Ok(());
    }
    let object_type = if let Some(object_type) = context.object_type_cache.borrow().get(new_oid) {
        *object_type
    } else {
        let db = FileObjectDatabase::from_git_dir(context.git_dir, context.format);
        let object = db.read_object(new_oid).map_err(|err| match err {
            GitError::NotFound(_) => {
                format!("trying to write ref '{display_name}' with nonexistent object {new_oid}")
            }
            err => err.to_string(),
        })?;
        context
            .object_type_cache
            .borrow_mut()
            .insert(*new_oid, object.object_type);
        object.object_type
    };
    if update_ref_requires_commit(name) && object_type != ObjectType::Commit {
        return Err(format!(
            "trying to write non-commit object {new_oid} to branch '{display_name}'"
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
    let zero = zero_oid(context.format)?;
    if request.expected_oid == Some(&zero)
        && current.is_some()
        && find_case_insensitive_ref_conflict(context.store, &request.name)?.is_some()
    {
        return print_update_ref_stdin_case_conflict_rejection(
            context,
            &request.name,
            request.new_oid.to_string(),
            zero.to_string(),
            stdout,
        );
    }
    if let Some(expected_oid) = request.expected_oid
        && let Some(rejection) = update_ref_stdin_expected_rejection(
            context.store,
            context.format,
            &request.requested,
            &request.name,
            current.as_ref(),
            expected_oid,
            request.new_oid.to_string(),
        )?
    {
        print_update_ref_stdin_rejection(rejection, stdout)?;
        return Ok(());
    }
    if request.new_oid == zero {
        return update_ref_delete_stdin(context.store, context.format, &request.name, None);
    }
    if let Err(reason) = check_update_ref_new_value_cached(
        context,
        &request.name,
        &request.requested,
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
        eprintln!("error: {reason}");
        return Ok(());
    }
    // A directory/file refname conflict (e.g. creating `refs/heads/ref` while
    // `refs/heads/ref/foo` exists) rejects only this update under
    // --batch-updates rather than aborting the whole batch.
    if let Some(conflict) = context.store.refname_directory_conflict(&request.name)? {
        writeln!(
            stdout,
            "rejected {} {} {} refname conflict",
            request.name,
            request.new_oid,
            request
                .expected_oid
                .map(ObjectId::to_string)
                .unwrap_or_else(|| "(null)".to_string())
        )?;
        eprintln!(
            "error: cannot lock ref '{}': '{conflict}' exists; cannot create '{}'",
            request.name, request.name
        );
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
    requested: &str,
    name: &str,
    expected: Option<&ObjectId>,
    stdout: &mut dyn Write,
) -> Result<()> {
    let current = store.read_ref(name)?;
    if let Some(expected) = expected
        && let Some(rejection) = update_ref_stdin_expected_rejection(
            store,
            format,
            requested,
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
    store: &FileRefStore,
    format: ObjectFormat,
    requested: &str,
    name: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
    stdout: &mut dyn Write,
) -> Result<()> {
    if let Some(rejection) = update_ref_stdin_expected_rejection(
        store,
        format,
        requested,
        name,
        current,
        expected,
        "(null)".to_string(),
    )? {
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
        check_update_ref_stdin_expected_named(
            context.store,
            context.format,
            &request.requested,
            &request.name,
            current.as_ref(),
            expected_oid,
        )?;
    }
    if request.new_oid == zero_oid(context.format)? {
        return update_ref_delete_stdin_named(
            context.store,
            context.format,
            &request.requested,
            &request.name,
            None,
        );
    }
    check_update_ref_new_value_cached(context, &request.name, &request.requested, &request.new_oid)
        .map_err(|reason| {
            eprintln!("fatal: {reason}");
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
                committer: ref_reflog_committer(context.config),
                message: context.message.clone(),
            });
    let hook = ReferenceTransactionHookRunner::new(context.git_dir);
    // Keep the transaction precondition `None`: the old-value was already
    // checked above with the stdin-shaped rejection messages, and re-checking
    // here would change that error surface.
    let mut tx = context.store.transaction().with_hook(&hook);
    tx.update(RefUpdate {
        name: request.name,
        expected: None,
        new: RefTarget::Direct(request.new_oid),
        reflog,
    });
    tx.commit()
}

fn update_ref_effective_name(store: &FileRefStore, name: &str, deref: bool) -> Result<String> {
    Ok(update_ref_effective_ref(store, name, deref)?.effective)
}

/// The result of dereferencing a (possibly symbolic) ref for an update: the
/// `requested` name the user typed and the `effective` name the write lands on.
/// git reports the *requested* name in the `cannot lock ref '<requested>'`
/// prefix but the *effective* (final, possibly dangling) name in the
/// `unable to resolve reference '<effective>'` reason — so a `symref -> foo`
/// with a missing `foo` yields
/// `cannot lock ref 'symref': unable to resolve reference 'foo'`.
struct EffectiveRefName {
    requested: String,
    effective: String,
}

fn update_ref_effective_ref(
    store: &FileRefStore,
    name: &str,
    deref: bool,
) -> Result<EffectiveRefName> {
    let requested = name.to_string();
    if !deref {
        return Ok(EffectiveRefName {
            effective: requested.clone(),
            requested,
        });
    }
    let mut current = requested.clone();
    for _ in 0..16 {
        match store.read_ref(&current) {
            Ok(Some(RefTarget::Symbolic(target))) => current = target,
            Ok(_) => break,
            Err(GitError::InvalidPath(_))
                if sley_refs::validate_ref_name_for_update(&current).is_ok() =>
            {
                break;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(EffectiveRefName {
        requested,
        effective: current,
    })
}

fn update_ref_delete(
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    name: &str,
    expected: Option<&ObjectId>,
    message: &[u8],
    create_reflog: bool,
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
    // Capture the deleted branch's tip and HEAD's pre-delete symref target so a
    // deletion of the branch HEAD points at can be mirrored into HEAD's reflog
    // (git logs the delete on the symref even though the branch's own reflog is
    // unlinked).
    let deleted_oid = match current.as_ref() {
        Some(RefTarget::Direct(oid)) => Some(*oid),
        _ => None,
    };
    let head_target = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => Some(target),
        _ => None,
    };
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
    if let Some(deleted_oid) = deleted_oid
        && head_target.as_deref() == Some(name)
        && update_ref_should_write_reflog(git_dir, "HEAD", create_reflog)?
    {
        store.append_reflog(
            "HEAD",
            &ReflogEntry {
                old_oid: deleted_oid,
                new_oid: zero_oid(format)?,
                committer: ref_reflog_committer(config),
                message: message.to_vec(),
            },
        )?;
    }
    Ok(())
}

fn update_ref_delete_stdin(
    store: &FileRefStore,
    format: ObjectFormat,
    name: &str,
    expected: Option<&ObjectId>,
) -> Result<()> {
    update_ref_delete_stdin_named(store, format, name, name, expected)
}

/// Delete `effective` (the dereferenced ref) while reporting `requested` (the
/// ref the user typed) in `cannot lock ref '<requested>'` — see
/// [`EffectiveRefName`].
fn update_ref_delete_stdin_named(
    store: &FileRefStore,
    format: ObjectFormat,
    requested: &str,
    effective: &str,
    expected: Option<&ObjectId>,
) -> Result<()> {
    let current = store.read_ref(effective)?;
    if let Some(expected) = expected {
        let zero = zero_oid(format)?;
        if expected != &zero {
            match current.as_ref() {
                Some(RefTarget::Direct(actual)) if actual == expected => {}
                Some(RefTarget::Direct(actual)) => {
                    return update_ref_stdin_lock_failure(
                        requested,
                        &format!("is at {actual} but expected {expected}"),
                    );
                }
                Some(RefTarget::Symbolic(_)) => {
                    // no-deref delete over a symref: git resolves one chain to an
                    // OID and compares it, reporting the symref name.
                    match resolve_ref_peeled(store, effective)? {
                        Some(actual) if &actual == expected => {}
                        Some(actual) => {
                            return update_ref_stdin_lock_failure(
                                requested,
                                &format!("is at {actual} but expected {expected}"),
                            );
                        }
                        None => {
                            return update_ref_stdin_lock_failure(
                                requested,
                                &format!("reference is missing but expected {expected}"),
                            );
                        }
                    }
                }
                None => {
                    return update_ref_stdin_lock_failure(
                        requested,
                        &format!("unable to resolve reference '{effective}'"),
                    );
                }
            }
        }
    }
    if current.is_some() {
        match current {
            Some(RefTarget::Symbolic(_)) => {
                store.delete_symbolic_ref(effective)?;
            }
            _ => {
                store.delete_ref(effective)?;
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
    let context = sley_config::ConfigIncludeContext::new(
        Some(sley_config::git_dir_for_include_context(&common_git_dir)),
        sley_config::repo_current_branch_name(&common_git_dir),
    );
    let Ok(config) = sley_config::load_effective_config(&common_git_dir, &context) else {
        return Ok(false);
    };
    if let Some(value) = config.get("core", None, "logAllRefUpdates") {
        return Ok(update_ref_log_all_ref_updates_matches(name, value));
    }
    if config.get_bool("core", None, "bare").unwrap_or(false) {
        return Ok(false);
    }
    Ok(update_ref_log_all_ref_updates_matches(name, "true"))
}

fn ref_reflog_committer(config: &GitConfig) -> Vec<u8> {
    let date = match env::var("GIT_COMMITTER_DATE") {
        Ok(date) if !date.is_empty() => date,
        _ => format!("@{} +0000", current_unix_seconds().max(1)),
    };
    commit_identity_from_env_with_date("COMMITTER", &date, config).unwrap_or_else(|_| {
        let timestamp = current_unix_seconds().max(1);
        format!("Git Rs <sley@example.invalid> {timestamp} +0000").into_bytes()
    })
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
    context: &UpdateRefStdinContext<'_>,
    command: &str,
    refname: &str,
    field: &str,
    value: &str,
) -> Result<ObjectId> {
    if let Some(oid) = context.oid_cache.borrow().get(value).copied() {
        return Ok(oid);
    }
    match parse_update_ref_oidish(context.git_dir, context.format, context.store, value) {
        Some(oid) => Ok(oid),
        None => {
            eprintln!("fatal: {command} {refname}: invalid {field}: {value}");
            Err(GitError::Exit(128))
        }
    }
    .inspect(|oid| {
        context
            .oid_cache
            .borrow_mut()
            .insert(value.to_string(), *oid);
    })
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

/// Check a stdin old-OID precondition with the requested name (`cannot lock ref
/// '<requested>'`) distinguished from the effective, dereferenced name (`unable
/// to resolve reference '<effective>'`). git uses the requested ref in the
/// lock-failure prefix and the final dangling target in the resolve-failure
/// reason; a non-symbolic update has them equal.
///
/// A `Symbolic` current arises only under `no-deref`: git still resolves the
/// symref one level to compare the OID (`refs_read_ref_full`), so a `no-deref`
/// update of a symref reports `is at <peeled> but expected <X>` (or `reference
/// is missing but expected <X>` when the symref dangles), naming the symref —
/// not `unable to resolve`.
fn check_update_ref_stdin_expected_named(
    store: &FileRefStore,
    format: ObjectFormat,
    requested: &str,
    effective: &str,
    current: Option<&RefTarget>,
    expected: &ObjectId,
) -> Result<()> {
    let zero = zero_oid(format)?;
    if expected == &zero {
        if matches!(current, Some(RefTarget::Symbolic(_)))
            && resolve_ref_peeled(store, effective)?.is_none()
        {
            return update_ref_stdin_lock_failure(requested, "dangling symref already exists");
        }
        if current.is_some() {
            return update_ref_stdin_lock_failure(requested, "reference already exists");
        }
        return Ok(());
    }

    match current {
        Some(RefTarget::Direct(actual)) if actual == expected => Ok(()),
        Some(RefTarget::Direct(actual)) => update_ref_stdin_lock_failure(
            requested,
            &format!("is at {actual} but expected {expected}"),
        ),
        Some(RefTarget::Symbolic(_)) => {
            // no-deref over a symref: resolve one chain to an OID and compare.
            match resolve_ref_peeled(store, effective)? {
                Some(actual) if &actual == expected => Ok(()),
                Some(actual) => update_ref_stdin_lock_failure(
                    requested,
                    &format!("is at {actual} but expected {expected}"),
                ),
                None => update_ref_stdin_lock_failure(
                    requested,
                    &format!("reference is missing but expected {expected}"),
                ),
            }
        }
        None => update_ref_stdin_lock_failure(
            requested,
            &format!("unable to resolve reference '{effective}'"),
        ),
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

pub(crate) fn cmd_show_ref(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
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
                if setup_show_ref_short_options(
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
        let refs = store.list_refs()?;
        return cmd_show_ref_exclude_existing(
            &refs,
            (!pattern.is_empty()).then_some(pattern.as_str()),
        );
    }
    if exists {
        if filters.len() != 1 {
            return show_ref_exists_requires_reference(filters.len());
        }
        if show_ref_exists(&store, filters[0])? {
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
                let oid =
                    resolve_revision(&git_dir, format, "HEAD", cli_session.replace_objects())?;
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
    let refs = store.list_refs()?;
    if include_head
        && let Ok(oid) = resolve_revision(&git_dir, format, "HEAD", cli_session.replace_objects())
    {
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
    if !matched {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn show_ref_exists(store: &FileRefStore, name: &str) -> Result<bool> {
    store.raw_ref_exists(name)
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

pub(crate) fn cmd_symbolic_ref(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
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
        [name, target] => update_symbolic_ref(
            &git_dir,
            &store,
            format,
            &identity_effective_config_for(cli_session).unwrap_or_default(),
            name,
            target,
            message,
        ),
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
    config: &GitConfig,
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
    let new_oid = if sley_refs::validate_ref_name_for_read(target).is_ok() {
        resolve_symbolic_ref_oid(store, format, target)?
    } else {
        zero_oid(format)?
    };
    let reflog = symbolic_ref_should_write_reflog(git_dir, name)?.then(|| ReflogEntry {
        old_oid,
        new_oid,
        committer: ref_reflog_committer(config),
        message,
    });
    let hook = ReferenceTransactionHookRunner::new(git_dir);
    let mut tx = store.transaction().with_hook(&hook);
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
            let detail = parse_df_conflict_message(&message)
                .filter(|(new_ref, existing_ref)| existing_ref.starts_with(&format!("{new_ref}/")))
                .and_then(|_| message.split_once(": ").map(|(_, detail)| detail))
                .unwrap_or(&message);
            eprintln!("error: {detail}");
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
    match resolve_ref_peeled(store, name) {
        Err(_) => zero_oid(format),
        Ok(None) => zero_oid(format),
        Ok(Some(oid)) => Ok(oid),
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
pub(crate) fn cmd_refs(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        eprintln!("error: need a subcommand");
        print_refs_usage();
        return Err(GitError::Exit(129));
    };
    match subcommand {
        "list" => {
            let git_dir = cli_session.git_dir()?;
            let config = identity_effective_config_for(cli_session).unwrap_or_default();
            commands::for_each_ref::for_each_ref_core_with_config(
                cli_session,
                &git_dir,
                &args[1..],
                "git refs list",
                &config,
            )
        }
        "exists" => cmd_refs_exists(cli_session, &args[1..]),
        "verify" => commands::refs_verify::cmd_refs_verify(cli_session, &args[1..]),
        "migrate" => cmd_refs_migrate(cli_session, &args[1..]),
        "optimize" => commands::pack::cmd_pack_refs(cli_session, &args[1..]),
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

fn cmd_refs_migrate(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut target_format = None::<RefStorageFormat>;
    let mut dry_run = false;
    let mut include_reflogs = true;
    let mut positionals = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "--reflog" => include_reflogs = true,
            "--no-reflog" => include_reflogs = false,
            "--ref-format" => {
                let Some(value) = iter.next() else {
                    eprintln!("usage: missing --ref-format=<format>");
                    return Err(GitError::Exit(129));
                };
                target_format = Some(parse_refs_migrate_ref_format(value)?);
            }
            value if value.starts_with("--ref-format=") => {
                let value = value
                    .strip_prefix("--ref-format=")
                    .ok_or_else(|| GitError::Command("--ref-format requires a value".into()))?;
                target_format = Some(parse_refs_migrate_ref_format(value)?);
            }
            "--" => {
                positionals.extend(iter.cloned());
                break;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                print_refs_usage();
                return Err(GitError::Exit(129));
            }
            value => positionals.push(value.to_string()),
        }
    }

    if !positionals.is_empty() {
        eprintln!("usage: too many arguments");
        return Err(GitError::Exit(129));
    }
    let Some(target_format) = target_format else {
        eprintln!("usage: missing --ref-format=<format>");
        return Err(GitError::Exit(129));
    };

    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let outcome = match sley_refs::migration::migrate_ref_storage(
        sley_refs::migration::MigrateRefStorageOptions {
            git_dir: &git_dir,
            common_git_dir: &common_git_dir,
            object_format: format,
            target_format,
            include_reflogs,
            dry_run,
        },
    ) {
        Ok(outcome) => outcome,
        Err(sley_refs::migration::MigrateRefStorageError::AlreadyUses(format)) => {
            eprintln!("error: repository already uses '{}' format", format.name());
            return Err(GitError::Exit(1));
        }
        Err(sley_refs::migration::MigrateRefStorageError::LinkedWorktreesUnsupported) => {
            eprintln!("error: migrating repositories with worktrees is not supported yet");
            return Err(GitError::Exit(1));
        }
        Err(sley_refs::migration::MigrateRefStorageError::Storage(err)) => return Err(err),
    };
    if let Some(migration_dir) = outcome.dry_run_path {
        println!(
            "Finished dry-run migration of refs, the result can be found at '{}'",
            refs_migrate_display_path(cli_session, &git_dir, &common_git_dir, &migration_dir)
        );
    }
    Ok(())
}

fn parse_refs_migrate_ref_format(value: &str) -> Result<RefStorageFormat> {
    match RefStorageFormat::parse(value) {
        Ok(format) => Ok(format),
        Err(_) => {
            eprintln!("error: unknown ref storage format '{value}'");
            Err(GitError::Exit(1))
        }
    }
}

fn refs_migrate_display_path(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    common_git_dir: &Path,
    path: &Path,
) -> String {
    if let Ok(worktree) = worktree_root_for_git_dir(cli_session, git_dir) {
        let dot_git = worktree.join(".git");
        if paths_refer_to_same_dir(common_git_dir, &dot_git)
            && let Ok(relative) = path.strip_prefix(common_git_dir)
        {
            return Path::new(".git").join(relative).display().to_string();
        }
    }
    path.display().to_string()
}

/// `git refs exists <ref>` — exit 0 if the raw ref exists, 2 if it does not
/// (ENOENT/EISDIR), matching builtin/refs.c::cmd_refs_exists. Does not DWIM and
/// does not read the pointed-at object.
fn cmd_refs_exists(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    // git: `argc != 1` after option parsing -> die. There are no options other
    // than -h, which parse_options would have consumed in cmd_refs already.
    let refs: Vec<&String> = args.iter().filter(|arg| arg.as_str() != "--").collect();
    if refs.len() != 1 {
        eprintln!("fatal: 'git refs exists' requires a reference");
        return Err(GitError::Exit(128));
    }
    let name = refs[0];
    let git_dir = cli_session.git_dir()?;
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
