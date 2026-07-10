//! `git diagnose` — bundle repository diagnostics into a zip archive.
//!
//! A faithful port of `builtin/diagnose.c` + `diagnose.c`'s
//! `create_diagnostics_archive`: collect version + repository + disk info, pack
//! statistics (`packs-local.txt`), and loose-object statistics
//! (`objects-local.txt`) into virtual files, optionally adding the raw `.git`
//! metadata for `--mode=all`, and write them as a zip of the empty tree.

use sley::plumbing::{sley_config, sley_core};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sley_archive::{ArchiveExtraEntry, ArchiveExtras, ZipArchiveOptions};

use crate::*;

const USAGE: &str = "usage: git diagnose [(-o | --output-directory) <path>] [(-s | --suffix) <format>]\n             [--mode=<mode>]";

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagnoseMode {
    Stats,
    All,
}

pub(crate) fn cmd_diagnose(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut output: Option<String> = None;
    let mut suffix = "%Y-%m-%d-%H%M".to_string();
    let mut mode = DiagnoseMode::Stats;

    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Err(GitError::Exit(129));
            }
            "-o" | "--output-directory" => {
                output = Some(take_value(args, &mut index, "output-directory")?);
            }
            value if value.starts_with("--output-directory=") => {
                output = Some(value["--output-directory=".len()..].to_string());
                index += 1;
            }
            "-s" | "--suffix" => {
                suffix = take_value(args, &mut index, "suffix")?;
            }
            value if value.starts_with("--suffix=") => {
                suffix = value["--suffix=".len()..].to_string();
                index += 1;
            }
            value if value.starts_with("--mode=") => {
                mode = parse_mode(&value["--mode=".len()..])?;
                index += 1;
            }
            "--mode" => {
                let value = take_value(args, &mut index, "mode")?;
                mode = parse_mode(&value)?;
            }
            other => {
                eprintln!("error: unknown option `{}'", other.trim_start_matches('-'));
                eprintln!("{USAGE}");
                return Err(GitError::Exit(129));
            }
        }
    }

    // Resolve the output path: `<output>/git-diagnostics-<suffix>.zip`.
    let mut zip_path = PathBuf::new();
    if let Some(output) = &output {
        zip_path.push(output);
    }
    let file_name = format!("git-diagnostics-{}.zip", strftime_suffix(&suffix));
    zip_path.push(file_name);

    if let Some(parent) = zip_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                GitError::InvalidFormat(format!(
                    "fatal: could not create leading directories for '{}': {err}",
                    zip_path.display()
                ))
            })?;
        }
    }

    create_diagnostics_archive(cli_session, &zip_path, mode)?;
    Ok(())
}

fn take_value(args: &[String], index: &mut usize, name: &str) -> Result<String> {
    match args.get(*index + 1) {
        Some(value) => {
            *index += 2;
            Ok(value.clone())
        }
        None => {
            eprintln!("error: option `{name}' requires a value");
            eprintln!("{USAGE}");
            Err(GitError::Exit(129))
        }
    }
}

fn parse_mode(value: &str) -> Result<DiagnoseMode> {
    match value {
        "stats" => Ok(DiagnoseMode::Stats),
        "all" => Ok(DiagnoseMode::All),
        other => {
            eprintln!("error: invalid --mode value '{other}'");
            Err(GitError::Exit(129))
        }
    }
}

/// Build the diagnostics zip, mirroring `create_diagnostics_archive`. The
/// human-readable header (version + repository root + disk info) is written to
/// stdout and also captured as the `diagnostics.log` virtual file.
fn create_diagnostics_archive(
    cli_session: &crate::session::CliSession,
    zip_path: &Path,
    mode: DiagnoseMode,
) -> Result<()> {
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let worktree = worktree_root_for_git_dir(cli_session, &git_dir)
        .ok()
        .unwrap_or_else(|| cwd.clone());

    let mut extras = ArchiveExtras::default();

    // diagnostics.log: version + repository root + disk info. Written to stdout
    // as the command's human-facing output, and added as a virtual file.
    let mut log = String::new();
    log.push_str("Collecting diagnostic info\n\n");
    log.push_str(&version_info());
    log.push_str(&format!("Repository root: {}\n", worktree.display()));
    log.push_str(&disk_info(&cwd));
    print!("{log}");
    let _ = std::io::stdout().flush();
    extras
        .files
        .push(virtual_file("diagnostics.log", log.into_bytes()));

    // packs-local.txt: object-directory file sizes.
    let objects_dir = git_dir.join("objects");
    extras.files.push(virtual_file(
        "packs-local.txt",
        pack_stats(&objects_dir).into_bytes(),
    ));

    // objects-local.txt: loose-object counts per fan-out.
    extras.files.push(virtual_file(
        "objects-local.txt",
        loose_object_stats(&objects_dir).into_bytes(),
    ));

    // --mode=all: include the raw `.git` metadata directories.
    if mode == DiagnoseMode::All {
        let archive_dirs: &[(&str, bool)] = &[
            (".git", false),
            (".git/hooks", false),
            (".git/info", false),
            (".git/logs", true),
            (".git/objects/info", false),
        ];
        for (dir, recurse) in archive_dirs {
            add_directory_to_extras(&mut extras, Path::new(dir), *recurse)?;
        }
    }

    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let empty_tree = ObjectId::empty_tree(format);
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let config = load_diagnose_config(&git_dir);
    let convert = sley_archive::ArchiveConvert::from_tree(
        &worktree,
        &git_dir,
        &config,
        &db,
        format,
        &empty_tree,
    )?;
    let options = ZipArchiveOptions {
        prefix: Vec::new(),
        strip_prefix: Vec::new(),
        mtime,
        commit_id: None,
        pathspecs: Vec::new(),
        compression_level: 6,
    };

    let mut file = fs::File::create(zip_path).map_err(|err| {
        GitError::InvalidFormat(format!(
            "fatal: unable to create diagnostics archive {}: {err}",
            zip_path.display()
        ))
    })?;
    sley_archive::write_zip_archive_full(
        &mut file,
        &db,
        format,
        &empty_tree,
        options,
        &convert,
        &extras,
    )?;

    eprintln!();
    eprintln!("Diagnostics complete.");
    eprintln!(
        "All of the gathered info is captured in '{}'",
        zip_path.display()
    );
    Ok(())
}

/// Load just enough config for the archive's attribute lookup (the empty tree
/// has no blobs, so this never actually converts anything, but the API needs a
/// config handle).
fn load_diagnose_config(git_dir: &Path) -> GitConfig {
    let context = sley_config::ConfigIncludeContext::new(
        Some(git_dir.to_path_buf()),
        sley_config::repo_current_branch_name(git_dir),
    );
    sley_config::load_pre_dispatch_config(Some(git_dir), &context).unwrap_or_default()
}

fn virtual_file(name: &str, content: Vec<u8>) -> ArchiveExtraEntry {
    ArchiveExtraEntry {
        path: name.as_bytes().to_vec(),
        content,
        mode: 0o100644,
    }
}

/// git's `get_version_info(buf, 1)` header, abbreviated to the version line.
fn version_info() -> String {
    format!("git version {}\n", sley_core::UPSTREAM_GIT_COMPAT_VERSION)
}

/// git's `get_disk_info`: report the available space at `path`. The exact byte
/// count requires `statvfs` (an unsafe syscall the workspace forbids), so the
/// path and a best-effort note are reported; the `Available space` prefix that
/// callers grep for is preserved.
fn disk_info(path: &Path) -> String {
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    format!("Available space on '{}': (not measured)\n", real.display())
}

/// git's `dir_file_stats`: `Contents of <objects-dir>:` followed by each file in
/// the pack directory and its size.
fn pack_stats(objects_dir: &Path) -> String {
    let mut out = format!("Contents of {}:\n", objects_dir.display());
    let pack_dir = objects_dir.join("pack");
    if let Ok(entries) = fs::read_dir(&pack_dir) {
        let mut files: Vec<(String, u64)> = entries
            .flatten()
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                if !metadata.is_file() {
                    return None;
                }
                Some((
                    entry.file_name().to_string_lossy().into_owned(),
                    metadata.len(),
                ))
            })
            .collect();
        files.sort();
        for (name, size) in files {
            out.push_str(&format!("{name:<70} {size:>16}\n"));
        }
    }
    out
}

/// git's `loose_objs_stats`: per-fanout loose-object counts plus a final
/// `Total: <n> loose objects` line.
fn loose_object_stats(objects_dir: &Path) -> String {
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return String::new();
    };
    let real = fs::canonicalize(objects_dir).unwrap_or_else(|_| objects_dir.to_path_buf());
    let mut out = format!("Object directory stats for {}:\n", real.display());
    let mut fanouts: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_fanout = name.len() == 2
                && name.bytes().all(|b| b.is_ascii_hexdigit())
                && entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
            is_fanout.then_some(name)
        })
        .collect();
    fanouts.sort();
    let mut total = 0;
    for fanout in fanouts {
        let count = fs::read_dir(objects_dir.join(&fanout))
            .map(|dir| {
                dir.flatten()
                    .filter(|entry| entry.metadata().map(|m| m.is_file()).unwrap_or(false))
                    .count()
            })
            .unwrap_or(0);
        total += count;
        out.push_str(&format!("{fanout} : {count:>7} files\n"));
    }
    out.push_str(&format!("Total: {total} loose objects"));
    out
}

/// git's `add_directory_to_archiver`: add every regular file under `dir`
/// (recursing into subdirectories when `recurse`), keying each as `<dir>/<name>`
/// so the archive reproduces the `.git` tree. Missing directories are skipped
/// with a warning, matching upstream.
fn add_directory_to_extras(extras: &mut ArchiveExtras, dir: &Path, recurse: bool) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "warning: could not archive missing directory '{}'",
                dir.display()
            );
            return Ok(());
        }
        Err(err) => {
            return Err(GitError::InvalidFormat(format!(
                "fatal: could not open directory '{}': {err}",
                dir.display()
            )));
        }
    };
    for entry in entries.flatten() {
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let child = dir.join(entry.file_name());
        if metadata.is_file() {
            let content = fs::read(&child).unwrap_or_default();
            extras.files.push(ArchiveExtraEntry {
                path: child.to_string_lossy().as_bytes().to_vec(),
                content,
                mode: file_mode(&metadata),
            });
        } else if metadata.is_dir() && recurse {
            add_directory_to_extras(extras, &child, recurse)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

/// Expand the strftime codes git's `strbuf_addftime` uses for the diagnose
/// suffix (`%Y %m %d %H %M %S` and `%%`). A suffix without `%` (the common case,
/// e.g. `-s test`) is returned verbatim.
fn strftime_suffix(suffix: &str) -> String {
    if !suffix.contains('%') {
        return suffix.to_string();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((now / 86_400) as i64);
    let secs_of_day = now % 86_400;
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let mut out = String::with_capacity(suffix.len());
    let mut chars = suffix.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{year:04}")),
            Some('m') => out.push_str(&format!("{month:02}")),
            Some('d') => out.push_str(&format!("{day:02}")),
            Some('H') => out.push_str(&format!("{hh:02}")),
            Some('M') => out.push_str(&format!("{mm:02}")),
            Some('S') => out.push_str(&format!("{ss:02}")),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Howard Hinnant's civil-from-days: convert a count of days since the Unix
/// epoch into `(year, month, day)` (proleptic Gregorian, UTC).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}
