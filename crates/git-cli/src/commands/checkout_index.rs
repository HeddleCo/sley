//! `git checkout-index` — copy files from the index to the working tree.
//!
//! Mirrors the plumbing command that materialises cache entries onto disk. The
//! supported surface covers the common real-`git` flags: `-a`/`--all`,
//! `-f`/`--force`, `-q`/`--quiet`, `-n`/`--no-create` (and `--create`),
//! `-u`/`--index`, `-z`, `--stdin`, `--prefix=<p>`,
//! `--ignore-skip-worktree-bits`, `--stage=<n>`, and explicit pathspecs. Output
//! text, the streams it lands on, and exit codes match upstream so the command
//! is a drop-in replacement.

// Pull shared plumbing (discover_git_dir, repository_object_format,
// worktree_root_for_git_dir, read_repo_config, FileObjectDatabase, ObjectReader,
// Index/IndexEntry, GitError/Result, std::* re-exports, …) from the crate root.
// A submodule can see its ancestors' items, so the glob keeps this file in step
// with whatever the root exposes without re-listing each name.
use crate::*;

/// Which index stage to copy out. Real `git` defaults to stage 0; `--stage=<n>`
/// selects a single conflict stage and `--stage=all` (handled separately) is not
/// part of the file-writing path covered here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckoutIndexStage {
    Single(u16),
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
            ignore_skip_worktree_bits: false,
            prefix: String::new(),
            stage: CheckoutIndexStage::Single(0),
            paths: Vec::new(),
        }
    }
}

pub(crate) fn cmd_checkout_index(args: &[String]) -> Result<()> {
    let options = parse_checkout_index_options(args)?;
    run_checkout_index(options)
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

fn parse_checkout_index_options(args: &[String]) -> Result<CheckoutIndexOptions> {
    let mut options = CheckoutIndexOptions::default();
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            options.paths.push(arg.as_bytes().to_vec());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => return Err(checkout_index_help()),
            "-a" | "--all" => options.all = true,
            "--no-all" => options.all = false,
            "-f" | "--force" => options.force = true,
            "--no-force" => options.force = false,
            "-q" | "--quiet" => options.quiet = true,
            "--no-quiet" => options.quiet = false,
            "-n" | "--no-create" => options.create = false,
            "--create" => options.create = true,
            "-u" | "--index" => options.update_stat = true,
            "--no-index" => options.update_stat = false,
            "-z" => options.nul = true,
            "--stdin" => options.stdin = true,
            "--no-stdin" => options.stdin = false,
            "--ignore-skip-worktree-bits" => options.ignore_skip_worktree_bits = true,
            "--no-ignore-skip-worktree-bits" => options.ignore_skip_worktree_bits = false,
            "--prefix" => {
                let value = iter
                    .next()
                    .ok_or_else(|| checkout_index_option_requires_value("prefix"))?;
                options.prefix = value.clone();
            }
            "--temp" | "--no-temp" => {
                return Err(GitError::Unsupported(
                    "checkout-index --temp is not supported".into(),
                ));
            }
            "--stage" => {
                let value = iter
                    .next()
                    .ok_or_else(|| checkout_index_option_requires_value("stage"))?;
                options.stage = parse_checkout_index_stage(value)?;
            }
            value if value.starts_with("--prefix=") => {
                options.prefix = value["--prefix=".len()..].to_string();
            }
            value if value.starts_with("--stage=") => {
                options.stage = parse_checkout_index_stage(&value["--stage=".len()..])?;
            }
            // Combined short flags, e.g. `-af` or `-afz`.
            value
                if value.starts_with('-')
                    && !value.starts_with("--")
                    && value.len() > 1
                    && value[1..]
                        .bytes()
                        .all(|byte| matches!(byte, b'a' | b'f' | b'q' | b'n' | b'u' | b'z')) =>
            {
                for byte in value[1..].bytes() {
                    match byte {
                        b'a' => options.all = true,
                        b'f' => options.force = true,
                        b'q' => options.quiet = true,
                        b'n' => options.create = false,
                        b'u' => options.update_stat = true,
                        b'z' => options.nul = true,
                        _ => unreachable!("short-option group was pre-filtered"),
                    }
                }
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(checkout_index_unknown_option(value));
            }
            value => options.paths.push(value.as_bytes().to_vec()),
        }
    }
    Ok(options)
}

fn parse_checkout_index_stage(value: &str) -> Result<CheckoutIndexStage> {
    match value {
        "1" => Ok(CheckoutIndexStage::Single(1)),
        "2" => Ok(CheckoutIndexStage::Single(2)),
        "3" => Ok(CheckoutIndexStage::Single(3)),
        "all" => Err(GitError::Unsupported(
            "checkout-index --stage=all is not supported".into(),
        )),
        _ => {
            eprintln!("fatal: stage should be between 1 and 3 or all");
            Err(GitError::Exit(128))
        }
    }
}

fn run_checkout_index(options: CheckoutIndexOptions) -> Result<()> {
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

    let cwd = env::current_dir()?;
    let git_dir = match discover_git_dir(&cwd) {
        Ok(dir) => dir,
        Err(_) => {
            eprintln!("fatal: not a git repository (or any of the parent directories): .git");
            return Err(GitError::Exit(128));
        }
    };
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;

    let mut index = match git_worktree::read_repository_index(&git_dir, format)? {
        Some(index) => index,
        None => Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        },
    };

    // The path prefix of the current directory relative to the worktree root is
    // prepended to explicit pathspecs for cache lookup (so `a.txt` from `sub/`
    // resolves to `sub/a.txt`), matching upstream.
    let dir_prefix = checkout_index_dir_prefix(&worktree_root, &cwd)?;

    let stdin_paths = if options.stdin {
        read_checkout_index_stdin(options.nul)?
    } else {
        Vec::new()
    };

    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let mut had_error = false;
    let mut wrote_index = false;

    let CheckoutIndexStage::Single(stage) = options.stage;

    if options.all {
        // Snapshot the entries we intend to write so we can mutate the index in
        // place for `-u` without iterator aliasing.
        let targets: Vec<usize> = index
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| checkout_index_entry_stage(entry) == stage)
            .filter(|(_, entry)| {
                options.ignore_skip_worktree_bits || !checkout_index_entry_skip_worktree(entry)
            })
            .filter(|(_, entry)| checkout_index_path_in_dir(&entry.path, &dir_prefix))
            .map(|(idx, _)| idx)
            .collect();
        for idx in targets {
            match checkout_one_index_entry(
                &worktree_root,
                &db,
                &config,
                &git_dir,
                format,
                &options,
                idx,
                &mut index,
            )? {
                CheckoutOutcome::Wrote => wrote_index |= options.update_stat,
                CheckoutOutcome::Skipped => {}
                CheckoutOutcome::Warned => had_error = true,
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
            let position = index.entries.iter().position(|entry| {
                entry.path == lookup && checkout_index_entry_stage(entry) == stage
            });
            let Some(idx) = position else {
                if !options.quiet {
                    eprintln!(
                        "git checkout-index: {} is not in the cache",
                        String::from_utf8_lossy(&lookup)
                    );
                }
                had_error = true;
                continue;
            };
            if !options.ignore_skip_worktree_bits
                && checkout_index_entry_skip_worktree(&index.entries[idx])
            {
                continue;
            }
            match checkout_one_index_entry(
                &worktree_root,
                &db,
                &config,
                &git_dir,
                format,
                &options,
                idx,
                &mut index,
            )? {
                CheckoutOutcome::Wrote => wrote_index |= options.update_stat,
                CheckoutOutcome::Skipped => {}
                CheckoutOutcome::Warned => had_error = true,
            }
        }
    }

    if wrote_index {
        index.entries.sort_by(|left, right| {
            left.path.cmp(&right.path).then_with(|| {
                checkout_index_entry_stage(left).cmp(&checkout_index_entry_stage(right))
            })
        });
        fs::write(
            git_worktree::repository_index_path(&git_dir),
            index.write(format)?,
        )?;
    }

    if had_error {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

enum CheckoutOutcome {
    Wrote,
    Skipped,
    Warned,
}

#[allow(clippy::too_many_arguments)]
fn checkout_one_index_entry(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    config: &GitConfig,
    git_dir: &Path,
    format: ObjectFormat,
    options: &CheckoutIndexOptions,
    index: usize,
    index_data: &mut Index,
) -> Result<CheckoutOutcome> {
    let entry = index_data.entries[index].clone();
    let dest = checkout_index_output_path(worktree_root, &options.prefix, &entry.path)?;

    let metadata = fs::symlink_metadata(&dest).ok();
    let exists = metadata.is_some();
    if let Some(metadata) = &metadata {
        if !options.force {
            // Without --force git silently leaves files that already match the
            // index (stat is up to date) and warns for any that differ.
            if checkout_index_entry_up_to_date(&entry, metadata) {
                return Ok(CheckoutOutcome::Skipped);
            }
            if !options.quiet {
                eprintln!(
                    "{}{} already exists, no checkout",
                    options.prefix,
                    String::from_utf8_lossy(&entry.path)
                );
            }
            return Ok(CheckoutOutcome::Warned);
        }
    }
    if !exists && !options.create {
        // `--no-create` suppresses creation of files that are not already there.
        return Ok(CheckoutOutcome::Skipped);
    }

    let object = db.read_object(&entry.oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            entry.oid,
            object.object_type.as_str()
        )));
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    // Mode 0o120000 is a symlink; everything else is a regular file (executable
    // when the mode carries the 0o111 bits).
    if entry.mode == 0o120000 {
        write_checkout_symlink(&dest, &object.body, exists)?;
    } else {
        let body = git_worktree::apply_smudge_filter(
            worktree_root,
            git_dir,
            format,
            config,
            &entry.path,
            &object.body,
        )?;
        write_checkout_regular_file(&dest, &body, entry.mode)?;
    }

    if options.update_stat {
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
        fs::remove_file(dest)?;
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
        fs::remove_file(dest)?;
    }
    fs::write(dest, target)?;
    Ok(())
}

fn write_checkout_regular_file(dest: &Path, body: &[u8], mode: u32) -> Result<()> {
    // Replace any existing symlink with a real file rather than writing through
    // it, mirroring git's create-or-truncate semantics.
    if let Ok(metadata) = fs::symlink_metadata(dest) {
        if metadata.file_type().is_symlink() {
            fs::remove_file(dest)?;
        }
    }
    fs::write(dest, body)?;
    apply_checkout_file_mode(dest, mode)?;
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
    if dir_prefix.is_empty() {
        return spec.to_vec();
    }
    let mut joined = dir_prefix.to_vec();
    joined.extend_from_slice(spec);
    joined
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
