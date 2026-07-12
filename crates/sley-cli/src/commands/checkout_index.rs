//! `git checkout-index` — copy files from the index to the working tree.
//!
//! Mirrors the plumbing command that materialises cache entries onto disk. The
//! supported surface covers the common real-`git` flags: `-a`/`--all`,
//! `-f`/`--force`, `-q`/`--quiet`, `-n`/`--no-create` (and `--create`),
//! `-u`/`--index`, `-z`, `--stdin`, `--prefix=<p>`,
//! `--ignore-skip-worktree-bits`, `--stage=<n>`, and explicit pathspecs. Output
//! text, the streams it lands on, and exit codes match upstream so the command
//! is a drop-in replacement.
#![allow(clippy::expect_used)]

use sley::plumbing::sley_worktree;
// Pull shared plumbing (RepositoryContext, ObjectReader, Index/IndexEntry,
// GitError/Result, std::* re-exports, …) from the crate root.
// A submodule can see its ancestors' items, so the glob keeps this file in step
// with whatever the root exposes without re-listing each name.
use crate::commands::cli_options::{last_tri_state_bool, opt_bool, opt_str};
use crate::*;
use sley_options::{OptionSpec, ParsedValue, parse_options};

/// Which index stage to copy out. Real `git` defaults to stage 0; `--stage=<n>`
/// selects a single conflict stage and `--stage=all` (handled separately) is not
/// part of the file-writing path covered here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckoutIndexStage {
    Single(u16),
    All,
}

/// Parsed command-line state for `git checkout-index`.
#[derive(Debug)]
struct CheckoutIndexOptions {
    all: bool,
    force: bool,
    quiet: bool,
    create: bool,
    update_stat: bool,
    nul: bool,
    stdin: bool,
    temp: Option<bool>,
    ignore_skip_worktree_bits: bool,
    prefix: String,
    stage: CheckoutIndexStage,
    paths: Vec<Vec<u8>>,
}

impl Default for CheckoutIndexOptions {
    fn default() -> Self {
        Self {
            all: false,
            force: false,
            quiet: false,
            create: true,
            update_stat: false,
            nul: false,
            stdin: false,
            temp: None,
            ignore_skip_worktree_bits: false,
            prefix: String::new(),
            stage: CheckoutIndexStage::Single(0),
            paths: Vec::new(),
        }
    }
}

const CHECKOUT_INDEX_USAGE_LINES: &[&str] = &["git checkout-index [<options>] [--] [<file>...]"];

fn checkout_index_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            Some('a'),
            Some("all"),
            sley_options::OptFlags::NONE,
            "check out all files in the index",
        ),
        opt_bool(
            None,
            Some("ignore-skip-worktree-bits"),
            sley_options::OptFlags::NONE,
            "do not skip files with skip-worktree set",
        ),
        opt_bool(
            Some('f'),
            Some("force"),
            sley_options::OptFlags::NONE,
            "force overwrite of existing files",
        ),
        opt_bool(
            Some('q'),
            Some("quiet"),
            sley_options::OptFlags::NONE,
            "no warning for existing files and files not in index",
        ),
        opt_bool(
            Some('n'),
            Some("no-create"),
            sley_options::OptFlags::NONE,
            "don't checkout new files",
        ),
        opt_bool(
            None,
            Some("create"),
            sley_options::OptFlags::NONE,
            "opposite of --no-create",
        ),
        opt_bool(
            Some('u'),
            Some("index"),
            sley_options::OptFlags::NONE,
            "update stat information in the index file",
        ),
        opt_bool(
            Some('z'),
            None,
            sley_options::OptFlags::NONE,
            "paths are separated with NUL character",
        ),
        opt_bool(
            None,
            Some("stdin"),
            sley_options::OptFlags::NONE,
            "read list of paths from the standard input",
        ),
        opt_bool(
            None,
            Some("temp"),
            sley_options::OptFlags::NONE,
            "write the content to temporary files",
        ),
        opt_str(
            None,
            Some("prefix"),
            "string",
            sley_options::OptFlags::NONE,
            "when creating files, prepend <string>",
        ),
        opt_str(
            None,
            Some("stage"),
            "n",
            sley_options::OptFlags::NONE,
            "copy out the files from named stage",
        ),
    ];
    SPECS
}

pub(crate) fn cmd_checkout_index(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = setup_checkout_index_options(args)?;
    run_checkout_index(cli_session, options)
}

/// Stage bits live in the upper nibble of the index entry flags.
fn checkout_index_entry_stage(entry: &IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

/// Extended flag bit 0x4000 marks skip-worktree entries (only meaningful when the
/// base `extended` flag is set, matching the on-disk index encoding).
fn checkout_index_entry_skip_worktree(entry: &IndexEntry) -> bool {
    const INDEX_FLAG_EXTENDED: u16 = 0x4000;
    const INDEX_EXTENDED_FLAG_SKIP_WORKTREE: u16 = 0x4000;
    entry.flags & INDEX_FLAG_EXTENDED != 0
        && entry.flags_extended & INDEX_EXTENDED_FLAG_SKIP_WORKTREE != 0
}

fn setup_checkout_index_options(args: &[String]) -> Result<CheckoutIndexOptions> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Err(checkout_index_help());
    }
    let parsed = match parse_options(
        args,
        checkout_index_option_specs(),
        CHECKOUT_INDEX_USAGE_LINES,
    ) {
        Ok(parsed) => parsed,
        Err(error) => {
            if let Some(message) = error.message() {
                eprintln!("error: {message}");
            }
            print_checkout_index_usage();
            return Err(GitError::Exit(129));
        }
    };
    let mut options = CheckoutIndexOptions::default();
    options.all = parsed.last_bool("all", false);
    options.force = parsed.last_bool("force", false);
    options.quiet = parsed.last_bool("quiet", false);
    options.update_stat = parsed.last_bool("index", false);
    options.nul = parsed
        .options
        .iter()
        .any(|option| option.short == Some('z'));
    options.stdin = parsed.last_bool("stdin", false);
    options.ignore_skip_worktree_bits = parsed.last_bool("ignore-skip-worktree-bits", false);
    let mut create = true;
    for option in &parsed.options {
        match (option.short, option.long) {
            (Some('n'), _) | (_, Some("no-create")) => {
                if let ParsedValue::Bool(value) = option.value {
                    create = !value;
                }
            }
            (_, Some("create")) => {
                if let ParsedValue::Bool(value) = option.value {
                    create = value;
                }
            }
            _ => {}
        }
    }
    options.create = create;
    options.temp = last_tri_state_bool(&parsed, "temp");
    if let Some(prefix) = parsed.last_str("prefix") {
        options.prefix = prefix.to_string();
    }
    if let Some(stage) = parsed.last_str("stage") {
        options.stage = parse_checkout_index_stage(stage)?;
    }
    options.paths = parsed
        .positionals
        .iter()
        .map(|path| path.as_bytes().to_vec())
        .collect();
    Ok(options)
}

fn parse_checkout_index_stage(value: &str) -> Result<CheckoutIndexStage> {
    match value {
        "1" => Ok(CheckoutIndexStage::Single(1)),
        "2" => Ok(CheckoutIndexStage::Single(2)),
        "3" => Ok(CheckoutIndexStage::Single(3)),
        "all" => Ok(CheckoutIndexStage::All),
        _ => {
            eprintln!("fatal: stage should be between 1 and 3 or all");
            Err(GitError::Exit(128))
        }
    }
}

fn run_checkout_index(
    cli_session: &crate::session::CliSession,
    options: CheckoutIndexOptions,
) -> Result<()> {
    if options.all && !options.paths.is_empty() {
        eprintln!("fatal: git checkout-index: don't mix '--all' and explicit filenames");
        return Err(GitError::Exit(128));
    }
    if options.stdin && !options.paths.is_empty() {
        eprintln!("fatal: git checkout-index: don't mix '--stdin' and explicit filenames");
        return Err(GitError::Exit(128));
    }
    if options.all && options.stdin {
        eprintln!("fatal: git checkout-index: don't mix '--all' and '--stdin'");
        return Err(GitError::Exit(128));
    }
    let temp = options
        .temp
        .unwrap_or(matches!(options.stage, CheckoutIndexStage::All));
    if matches!(options.stage, CheckoutIndexStage::All) && !temp {
        eprintln!("fatal: options '--stage=all' and '--no-temp' cannot be used together");
        return Err(GitError::Exit(128));
    }

    let repo = match RepositoryContext::from_session(cli_session) {
        Ok(repo) => repo,
        Err(GitError::NotFound(_)) => {
            eprintln!("fatal: not a git repository (or any of the parent directories): .git");
            return Err(GitError::Exit(128));
        }
        Err(err) => return Err(err),
    };
    let cwd = repo.cwd();
    let git_dir = repo.git_dir();
    let format = repo.format();
    let config = repo.config();
    let db = repo.objects();
    let worktree_root = repo.worktree_root()?;

    let mut index = match sley_worktree::read_repository_index(git_dir, format)? {
        Some(index) => index,
        None => Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        },
    };
    // checkout-index addresses logical cache entries. Keep the serialized
    // sparse-directory names only for Git's special explicit-directory error,
    // then expand a command-local view so `--all` and
    // `--ignore-skip-worktree-bits` can see the represented leaves without an
    // observable full-index transition or an on-disk rewrite.
    let sparse_directory_paths = index
        .entries
        .iter()
        .filter(|entry| entry.is_sparse_dir())
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect::<BTreeSet<_>>();
    let physical_sparse_index = (!sparse_directory_paths.is_empty()).then(|| index.clone());
    sley_worktree::expand_sparse_index_view(&mut index, db, format)?;

    // The path prefix of the current directory relative to the worktree root is
    // prepended to explicit pathspecs for cache lookup (so `a.txt` from `sub/`
    // resolves to `sub/a.txt`), matching upstream.
    let dir_prefix = checkout_index_dir_prefix(worktree_root, cwd)?;

    let stdin_paths = if options.stdin {
        read_checkout_index_stdin(options.nul)?
    } else {
        Vec::new()
    };

    let mut had_error = false;
    let mut wrote_index = false;
    let checkout_context = CheckoutIndexContext {
        worktree_root,
        db,
        config,
        git_dir,
        format,
        options: &options,
    };

    if options.all {
        // Snapshot the entries we intend to write so we can mutate the index in
        // place for `-u` without iterator aliasing.
        let targets: Vec<usize> = index
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| checkout_index_stage_matches(&options.stage, entry))
            .filter(|(_, entry)| {
                options.ignore_skip_worktree_bits || !checkout_index_entry_skip_worktree(entry)
            })
            .filter(|(_, entry)| checkout_index_path_in_dir(&entry.path, &dir_prefix))
            .map(|(idx, _)| idx)
            .collect();
        if temp {
            for group in checkout_index_group_targets(&targets, &index) {
                if checkout_temp_index_entries(&checkout_context, &dir_prefix, &group, &index)? {
                    had_error = true;
                }
            }
        } else {
            for idx in targets {
                match checkout_one_index_entry(&checkout_context, idx, &mut index)? {
                    CheckoutOutcome::Wrote => {
                        wrote_index |= options.update_stat && options.prefix.is_empty()
                    }
                    CheckoutOutcome::Skipped => {}
                    CheckoutOutcome::Warned => had_error = true,
                }
            }
        }
    } else {
        let requested: Vec<Vec<u8>> = if options.stdin {
            stdin_paths
        } else {
            options.paths.clone()
        };
        for spec in &requested {
            let lookup = checkout_index_join_prefix(&dir_prefix, spec);
            let positions: Vec<usize> = index
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.path == lookup)
                .filter(|(_, entry)| checkout_index_stage_matches(&options.stage, entry))
                .map(|(idx, _)| idx)
                .collect();
            if positions.is_empty() {
                let mut sparse_lookup = lookup.clone();
                if !sparse_lookup.ends_with(b"/") {
                    sparse_lookup.push(b'/');
                }
                if sparse_directory_paths.contains(&sparse_lookup) {
                    if !options.quiet {
                        eprintln!(
                            "git checkout-index: {} is a sparse directory",
                            String::from_utf8_lossy(&sparse_lookup)
                        );
                    }
                    had_error = true;
                    continue;
                }
                if matches!(options.stage, CheckoutIndexStage::All)
                    && index.entries.iter().any(|entry| entry.path == lookup)
                {
                    continue;
                }
                if !options.quiet {
                    eprintln!(
                        "git checkout-index: {} is not in the cache",
                        String::from_utf8_lossy(&lookup)
                    );
                }
                had_error = true;
                continue;
            };
            let positions: Vec<usize> = positions
                .into_iter()
                .filter(|idx| {
                    options.ignore_skip_worktree_bits
                        || !checkout_index_entry_skip_worktree(&index.entries[*idx])
                })
                .collect();
            if positions.is_empty() {
                if !options.quiet {
                    eprintln!(
                        "git checkout-index: {} has skip-worktree enabled; use '--ignore-skip-worktree-bits' to checkout",
                        String::from_utf8_lossy(&lookup)
                    );
                }
                had_error = true;
                continue;
            }
            if temp {
                if checkout_temp_index_entries(&checkout_context, &dir_prefix, &positions, &index)?
                {
                    had_error = true;
                }
            } else {
                for idx in positions {
                    match checkout_one_index_entry(&checkout_context, idx, &mut index)? {
                        CheckoutOutcome::Wrote => {
                            wrote_index |= options.update_stat && options.prefix.is_empty()
                        }
                        CheckoutOutcome::Skipped => {}
                        CheckoutOutcome::Warned => had_error = true,
                    }
                }
            }
        }
    }

    if wrote_index {
        let mut index_to_write = if let Some(mut physical) = physical_sparse_index {
            // The expanded index is a semantic checkout view, not a new
            // storage layout. `checkout-index -u` updates cached stat data for
            // physical leaf entries only; collapsed directories remain
            // collapsed just as Git leaves them, and the `sdir` extension is
            // preserved.
            for entry in &mut physical.entries {
                if entry.is_sparse_dir() {
                    continue;
                }
                if let Some(updated) = index.entries.iter().find(|updated| {
                    updated.path == entry.path
                        && checkout_index_entry_stage(updated) == checkout_index_entry_stage(entry)
                }) {
                    copy_checkout_index_stat(entry, updated);
                }
            }
            physical
        } else {
            index
        };
        index_to_write.entries.sort_by(|left, right| {
            left.path.cmp(&right.path).then_with(|| {
                checkout_index_entry_stage(left).cmp(&checkout_index_entry_stage(right))
            })
        });
        fs::write(
            sley_worktree::repository_index_path(git_dir),
            index_to_write.write(format)?,
        )?;
    }

    if had_error {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn copy_checkout_index_stat(destination: &mut IndexEntry, source: &IndexEntry) {
    destination.ctime_seconds = source.ctime_seconds;
    destination.ctime_nanoseconds = source.ctime_nanoseconds;
    destination.mtime_seconds = source.mtime_seconds;
    destination.mtime_nanoseconds = source.mtime_nanoseconds;
    destination.dev = source.dev;
    destination.ino = source.ino;
    destination.uid = source.uid;
    destination.gid = source.gid;
    destination.size = source.size;
}

enum CheckoutOutcome {
    Wrote,
    Skipped,
    Warned,
}

struct CheckoutIndexContext<'a> {
    worktree_root: &'a Path,
    db: &'a FileObjectDatabase,
    config: &'a GitConfig,
    git_dir: &'a Path,
    format: ObjectFormat,
    options: &'a CheckoutIndexOptions,
}

fn checkout_index_stage_matches(stage: &CheckoutIndexStage, entry: &IndexEntry) -> bool {
    match stage {
        CheckoutIndexStage::Single(stage) => checkout_index_entry_stage(entry) == *stage,
        CheckoutIndexStage::All => checkout_index_entry_stage(entry) != 0,
    }
}

fn checkout_index_group_targets(targets: &[usize], index: &Index) -> Vec<Vec<usize>> {
    let mut groups = Vec::<Vec<usize>>::new();
    for idx in targets {
        let entry = &index.entries[*idx];
        match groups.last_mut() {
            Some(group) if index.entries[group[0]].path == entry.path => group.push(*idx),
            _ => groups.push(vec![*idx]),
        }
    }
    groups
}

fn checkout_temp_index_entries(
    context: &CheckoutIndexContext<'_>,
    dir_prefix: &[u8],
    positions: &[usize],
    index_data: &Index,
) -> Result<bool> {
    if positions.is_empty() {
        return Ok(false);
    }
    match context.options.stage {
        CheckoutIndexStage::Single(_) => {
            let entry = &index_data.entries[positions[0]];
            let temp = checkout_temp_write_entry(context, entry)?;
            checkout_index_print_temp_record(
                context,
                &[Some(temp)],
                &checkout_index_relative_display(&entry.path, dir_prefix),
            )?;
        }
        CheckoutIndexStage::All => {
            let mut temps: [Option<String>; 3] = [None, None, None];
            for idx in positions {
                let entry = &index_data.entries[*idx];
                let stage = checkout_index_entry_stage(entry);
                if (1..=3).contains(&stage) {
                    temps[(stage - 1) as usize] = Some(checkout_temp_write_entry(context, entry)?);
                }
            }
            if temps.iter().any(Option::is_some) {
                let entry = &index_data.entries[positions[0]];
                checkout_index_print_temp_record(
                    context,
                    &temps,
                    &checkout_index_relative_display(&entry.path, dir_prefix),
                )?;
            }
        }
    }
    Ok(false)
}

fn checkout_temp_write_entry(
    context: &CheckoutIndexContext<'_>,
    entry: &IndexEntry,
) -> Result<String> {
    let object = context.db.read_object(&entry.oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            entry.oid,
            object.object_type.as_str()
        )));
    }
    let body = if entry.mode == 0o120000 {
        object.body.clone()
    } else {
        sley_worktree::apply_smudge_filter(
            context.worktree_root,
            context.git_dir,
            context.format,
            context.config,
            &entry.path,
            &object.body,
        )?
    };
    let (name, path) = checkout_temp_create_path(context.worktree_root)?;
    fs::write(&path, body)?;
    apply_checkout_file_mode(&path, entry.mode)?;
    Ok(name)
}

fn checkout_temp_create_path(worktree_root: &Path) -> Result<(String, PathBuf)> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let pid = std::process::id() as u64;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    for attempt in 0..4096_u64 {
        let mut value = nanos ^ pid.rotate_left(17) ^ attempt.wrapping_mul(0x9e3779b97f4a7c15);
        let mut suffix = [b'A'; 6];
        for slot in &mut suffix {
            *slot = alphabet[(value % alphabet.len() as u64) as usize];
            value /= alphabet.len() as u64;
        }
        let suffix = std::str::from_utf8(&suffix).expect("temporary suffix is ASCII");
        let name = format!(".merge_file_{suffix}");
        let path = worktree_root.join(&name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok((name, path)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(GitError::Io(
        "unable to create temporary checkout file".into(),
    ))
}

fn checkout_index_print_temp_record(
    context: &CheckoutIndexContext<'_>,
    temps: &[Option<String>],
    path: &str,
) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for (idx, temp) in temps.iter().enumerate() {
        if idx > 0 {
            stdout.write_all(b" ")?;
        }
        match temp {
            Some(temp) => stdout.write_all(temp.as_bytes())?,
            None => stdout.write_all(b".")?,
        }
    }
    stdout.write_all(b"\t")?;
    stdout.write_all(path.as_bytes())?;
    stdout.write_all(if context.options.nul { b"\0" } else { b"\n" })?;
    Ok(())
}

fn checkout_index_relative_display(path: &[u8], dir_prefix: &[u8]) -> String {
    if dir_prefix.is_empty() {
        return String::from_utf8_lossy(path).into_owned();
    }
    if let Some(rest) = path.strip_prefix(dir_prefix) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    let depth = dir_prefix.iter().filter(|byte| **byte == b'/').count();
    let mut display = Vec::new();
    for _ in 0..depth {
        display.extend_from_slice(b"../");
    }
    display.extend_from_slice(path);
    String::from_utf8_lossy(&display).into_owned()
}

fn checkout_one_index_entry(
    context: &CheckoutIndexContext<'_>,
    index: usize,
    index_data: &mut Index,
) -> Result<CheckoutOutcome> {
    let entry = index_data.entries[index].clone();
    let dest =
        checkout_index_output_path(context.worktree_root, &context.options.prefix, &entry.path)?;

    let metadata = fs::symlink_metadata(&dest).ok();
    let exists = metadata.is_some();
    if let Some(metadata) = &metadata
        && !context.options.force
    {
        // Without --force git silently leaves files that already match the
        // index (stat is up to date) and warns for any that differ.
        if checkout_index_entry_up_to_date(&entry, metadata) {
            return Ok(CheckoutOutcome::Skipped);
        }
        if !context.options.quiet {
            eprintln!(
                "{}{} already exists, no checkout",
                context.options.prefix,
                String::from_utf8_lossy(&entry.path)
            );
        }
        return Ok(CheckoutOutcome::Warned);
    }
    if !exists && !context.options.create {
        // `--no-create` suppresses creation of files that are not already there.
        return Ok(CheckoutOutcome::Skipped);
    }

    let object = context.db.read_object(&entry.oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            entry.oid,
            object.object_type.as_str()
        )));
    }

    if let Some(parent) = dest.parent() {
        let protected_prefix_parent =
            checkout_index_prefix_parent(context.worktree_root, &context.options.prefix)?;
        checkout_index_ensure_parent_dirs(
            parent,
            context.options.force,
            protected_prefix_parent.as_deref(),
        )?;
    }

    // Mode 0o120000 is a symlink; everything else is a regular file (executable
    // when the mode carries the 0o111 bits).
    if entry.mode == 0o120000 {
        write_checkout_symlink(&dest, &object.body, exists)?;
    } else {
        let body = sley_worktree::apply_smudge_filter(
            context.worktree_root,
            context.git_dir,
            context.format,
            context.config,
            &entry.path,
            &object.body,
        )?;
        write_checkout_regular_file(&dest, &body, entry.mode)?;
    }

    if context.options.update_stat {
        if let Ok(metadata) = fs::symlink_metadata(&dest) {
            checkout_index_refresh_stat(&mut index_data.entries[index], &metadata);
        }
        return Ok(CheckoutOutcome::Wrote);
    }
    Ok(CheckoutOutcome::Skipped)
}

/// Whether the worktree file at `dest` already matches the index entry's cached
/// stat data, mirroring git's `ie_match_stat`. When it matches, `checkout-index`
/// without `--force` leaves the file untouched and emits no warning.
#[cfg(unix)]
fn checkout_index_entry_up_to_date(entry: &IndexEntry, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // A type change (file <-> symlink, exec bit) always counts as modified.
    if !checkout_index_modes_match(entry.mode, metadata) {
        return false;
    }
    let mtime_secs = metadata.mtime().clamp(0, u32::MAX as i64) as u32;
    let mtime_nsecs = metadata.mtime_nsec().clamp(0, u32::MAX as i64) as u32;
    let ctime_secs = metadata.ctime().clamp(0, u32::MAX as i64) as u32;
    let ctime_nsecs = metadata.ctime_nsec().clamp(0, u32::MAX as i64) as u32;
    entry.mtime_seconds == mtime_secs
        && entry.mtime_nanoseconds == mtime_nsecs
        && entry.ctime_seconds == ctime_secs
        && entry.ctime_nanoseconds == ctime_nsecs
        && entry.dev == metadata.dev() as u32
        && entry.ino == metadata.ino() as u32
        && entry.uid == metadata.uid()
        && entry.gid == metadata.gid()
        && entry.size == (metadata.len().min(u32::MAX as u64) as u32)
}

#[cfg(not(unix))]
fn checkout_index_entry_up_to_date(entry: &IndexEntry, metadata: &fs::Metadata) -> bool {
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();
    entry.mtime_seconds == mtime.as_secs().min(u32::MAX as u64) as u32
        && entry.mtime_nanoseconds == mtime.subsec_nanos()
        && entry.size == (metadata.len().min(u32::MAX as u64) as u32)
}

/// Index mode (regular/exec/symlink) versus on-disk file type and exec bit.
#[cfg(unix)]
fn checkout_index_modes_match(entry_mode: u32, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let file_type = metadata.file_type();
    if entry_mode == 0o120000 {
        return file_type.is_symlink();
    }
    if !file_type.is_file() {
        return false;
    }
    let executable = metadata.permissions().mode() & 0o111 != 0;
    let entry_executable = entry_mode & 0o111 != 0;
    executable == entry_executable
}

#[cfg(unix)]
fn write_checkout_symlink(dest: &Path, target: &[u8], exists: bool) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if exists {
        checkout_index_remove_existing_path(dest)?;
    }
    let target = std::ffi::OsStr::from_bytes(target);
    std::os::unix::fs::symlink(target, dest)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_checkout_symlink(dest: &Path, target: &[u8], exists: bool) -> Result<()> {
    // No symlink support: fall back to writing the link text as a regular file,
    // matching git's behaviour on filesystems without symlink support.
    if exists {
        checkout_index_remove_existing_path(dest)?;
    }
    fs::write(dest, target)?;
    Ok(())
}

fn write_checkout_regular_file(dest: &Path, body: &[u8], mode: u32) -> Result<()> {
    // Replace any existing symlink with a real file rather than writing through
    // it, mirroring git's create-or-truncate semantics.
    if let Ok(metadata) = fs::symlink_metadata(dest)
        && (metadata.file_type().is_symlink() || metadata.is_dir())
    {
        checkout_index_remove_existing_path(dest)?;
    }
    fs::write(dest, body)?;
    apply_checkout_file_mode(dest, mode)?;
    Ok(())
}

/// Return the directory portion that belongs wholly to `--prefix`.
///
/// Existing symlinks in this portion are followed by Git: the prefix names an
/// output location supplied by the caller, rather than an index path being
/// materialized.  A symlink introduced only after concatenating an index path
/// is still replaced under `--force`.
fn checkout_index_prefix_parent(worktree_root: &Path, prefix: &str) -> Result<Option<PathBuf>> {
    let Some(slash) = prefix.as_bytes().iter().rposition(|byte| *byte == b'/') else {
        return Ok(None);
    };
    let directory = &prefix.as_bytes()[..slash];
    if directory.is_empty() {
        return Ok(None);
    }
    let directory =
        std::str::from_utf8(directory).map_err(|err| GitError::InvalidPath(err.to_string()))?;
    Ok(Some(worktree_root.join(directory)))
}

fn checkout_index_ensure_parent_dirs(
    path: &Path,
    force: bool,
    protected_prefix_parent: Option<&Path>,
) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.file_type().is_symlink()
            && protected_prefix_parent.is_some_and(|prefix| prefix.starts_with(path))
            && fs::metadata(path).is_ok_and(|target| target.is_dir())
        {
            return Ok(());
        }
        if !force {
            fs::create_dir_all(path)?;
            return Ok(());
        }
        checkout_index_remove_existing_path(path)?;
    }
    if let Some(parent) = path.parent() {
        checkout_index_ensure_parent_dirs(parent, force, protected_prefix_parent)?;
    }
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                Ok(())
            } else if force {
                checkout_index_remove_existing_path(path)?;
                fs::create_dir(path)?;
                Ok(())
            } else {
                Err(err.into())
            }
        }
        Err(err) => Err(err.into()),
    }
}

fn checkout_index_remove_existing_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn apply_checkout_file_mode(dest: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
    fs::set_permissions(dest, fs::Permissions::from_mode(perms))?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_checkout_file_mode(_dest: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Update the stat fields of an index entry from freshly written file metadata so
/// a subsequent `git status` / `git diff-files` treats it as up to date.
fn checkout_index_refresh_stat(entry: &mut IndexEntry, metadata: &fs::Metadata) {
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();
    entry.mtime_seconds = mtime.as_secs().min(u32::MAX as u64) as u32;
    entry.mtime_nanoseconds = mtime.subsec_nanos();
    entry.ctime_seconds = entry.mtime_seconds;
    entry.ctime_nanoseconds = entry.mtime_nanoseconds;
    entry.size = checkout_index_stat_size(metadata);
    checkout_index_apply_unix_stat(entry, metadata);
}

#[cfg(unix)]
fn checkout_index_stat_size(metadata: &fs::Metadata) -> u32 {
    // Symlinks store their target length; the index records the link text size.
    metadata.len().min(u32::MAX as u64) as u32
}

#[cfg(not(unix))]
fn checkout_index_stat_size(metadata: &fs::Metadata) -> u32 {
    metadata.len().min(u32::MAX as u64) as u32
}

#[cfg(unix)]
fn checkout_index_apply_unix_stat(entry: &mut IndexEntry, metadata: &fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    entry.ctime_seconds = metadata.ctime().min(u32::MAX as i64).max(0) as u32;
    entry.ctime_nanoseconds = metadata.ctime_nsec().min(u32::MAX as i64).max(0) as u32;
    entry.dev = metadata.dev() as u32;
    entry.ino = metadata.ino() as u32;
    entry.uid = metadata.uid();
    entry.gid = metadata.gid();
}

#[cfg(not(unix))]
fn checkout_index_apply_unix_stat(_entry: &mut IndexEntry, _metadata: &fs::Metadata) {}

/// Compute the on-disk destination: `<worktree>/<prefix><cache-path>`. At the
/// repository root (the usual invocation) this equals the cwd-relative path; the
/// prefix is a literal string prepended to the cache path before joining.
fn checkout_index_output_path(
    worktree_root: &Path,
    prefix: &str,
    cache_path: &[u8],
) -> Result<PathBuf> {
    let mut combined = prefix.as_bytes().to_vec();
    combined.extend_from_slice(cache_path);
    let text =
        std::str::from_utf8(&combined).map_err(|err| GitError::InvalidPath(err.to_string()))?;
    let relative = PathBuf::from(text);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(GitError::InvalidPath(format!(
            "invalid worktree path {text}"
        )));
    }
    Ok(worktree_root.join(relative))
}

/// Join the current-directory prefix in front of an explicit pathspec for cache
/// lookup. With an empty prefix (invoked from the worktree root) the pathspec is
/// used verbatim.
fn checkout_index_join_prefix(dir_prefix: &[u8], spec: &[u8]) -> Vec<u8> {
    let mut joined = dir_prefix.to_vec();
    joined.extend_from_slice(spec);
    checkout_index_normalize_cache_path(&joined)
}

fn checkout_index_normalize_cache_path(path: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(path);
    let mut parts = Vec::<&str>::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/").into_bytes()
}

/// `-a` from a subdirectory only checks out entries living under that directory.
fn checkout_index_path_in_dir(path: &[u8], dir_prefix: &[u8]) -> bool {
    dir_prefix.is_empty() || path.starts_with(dir_prefix)
}

/// The repository-relative path of `cwd`, with a trailing slash, or empty when
/// `cwd` is the worktree root.
fn checkout_index_dir_prefix(worktree_root: &Path, cwd: &Path) -> Result<Vec<u8>> {
    let canonical_root =
        fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    let canonical_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let Ok(relative) = canonical_cwd.strip_prefix(&canonical_root) else {
        return Ok(Vec::new());
    };
    if relative.as_os_str().is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    for component in relative.components() {
        if let std::path::Component::Normal(part) = component {
            if !parts.is_empty() {
                parts.push(b'/');
            }
            parts.extend_from_slice(part.to_string_lossy().as_bytes());
        }
    }
    if !parts.is_empty() {
        parts.push(b'/');
    }
    Ok(parts)
}

fn read_checkout_index_stdin(nul: bool) -> Result<Vec<Vec<u8>>> {
    let mut bytes = Vec::new();
    io::stdin().read_to_end(&mut bytes)?;
    let separator = if nul { b'\0' } else { b'\n' };
    Ok(bytes
        .split(|byte| *byte == separator)
        .filter_map(|raw| {
            let trimmed = if !nul && raw.ends_with(b"\r") {
                &raw[..raw.len() - 1]
            } else {
                raw
            };
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_vec())
            }
        })
        .collect())
}

fn checkout_index_option_requires_value(name: &str) -> GitError {
    eprintln!("error: option `{name}' requires a value");
    GitError::Exit(129)
}

fn checkout_index_unknown_option(value: &str) -> GitError {
    let display = value.strip_prefix("--").unwrap_or(value);
    eprintln!("error: unknown option `{display}'");
    print_checkout_index_usage();
    GitError::Exit(129)
}

fn checkout_index_help() -> GitError {
    print_checkout_index_usage_to_stdout();
    GitError::Exit(129)
}

const CHECKOUT_INDEX_USAGE: &str = "\
usage: git checkout-index [<options>] [--] [<file>...]

    -a, --[no-]all        check out all files in the index
    --[no-]ignore-skip-worktree-bits
                          do not skip files with skip-worktree set
    -f, --[no-]force      force overwrite of existing files
    -q, --[no-]quiet      no warning for existing files and files not in index
    -n, --no-create       don't checkout new files
    --create              opposite of --no-create
    -u, --[no-]index      update stat information in the index file
    -z                    paths are separated with NUL character
    --[no-]stdin          read list of paths from the standard input
    --[no-]temp           write the content to temporary files
    --[no-]prefix <string>
                          when creating files, prepend <string>
    --stage (1|2|3|all)   copy out the files from named stage

";

fn print_checkout_index_usage() {
    eprint!("{CHECKOUT_INDEX_USAGE}");
}

fn print_checkout_index_usage_to_stdout() {
    print!("{CHECKOUT_INDEX_USAGE}");
}
