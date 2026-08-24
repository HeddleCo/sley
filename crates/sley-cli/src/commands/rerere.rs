//! Native `git rerere` support — porcelain shell over the
//! [`sley_diff_merge::rerere`] engine.
//!
//! The byte-level machinery (MERGE_RR format, rr-cache layout, conflict-id
//! computation, remember/reuse logic) lives in sley-diff-merge, colocated with
//! the merge machinery like upstream's rerere.c. This module owns argv
//! parsing, usage text, stdout/stderr presentation, and the index-staging seam
//! that autoupdate needs.
#![allow(clippy::expect_used)]

use crate::commands::cli_options::opt_bool;
use crate::*;
use sley::plumbing::{sley_diff_merge, sley_worktree};
use sley_options::{OptionSpec, parse_options};
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerereSubcommand {
    Clear,
    Diff,
    Forget,
    Gc,
    Remaining,
    Status,
}

#[derive(Debug)]
struct RerereOptions {
    subcommand: Option<RerereSubcommand>,
    autoupdate: Option<bool>,
    paths: Vec<String>,
}

const RERERE_USAGE: &[&str] =
    &["git rerere [clear | forget <pathspec>... | diff | status | remaining | gc]"];

fn rerere_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[opt_bool(
        None,
        Some("rerere-autoupdate"),
        sley_options::OptFlags::NONE,
        "register clean resolutions in index",
    )];
    SPECS
}

pub(crate) fn cmd_rerere(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let options = setup_rerere_options(args)?;
    let _cwd = env::current_dir()?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    match options.subcommand {
        None => commands::rerere::repo_rerere(&git_dir, &worktree_root, format, options.autoupdate)
            .map(|_| ()),
        Some(RerereSubcommand::Status) => rerere_status(&git_dir),
        Some(RerereSubcommand::Remaining) => rerere_remaining(&git_dir, &worktree_root, format),
        Some(RerereSubcommand::Diff) => {
            rerere_diff(&git_dir, &worktree_root, format, cli_session.lazy_fetch())
        }
        Some(RerereSubcommand::Clear) => commands::rerere::rerere_clear(&git_dir),
        Some(RerereSubcommand::Forget) => rerere_forget(
            &git_dir,
            &options.paths,
            &mut sley_diff_merge::StderrRerereReporter,
        ),
        Some(RerereSubcommand::Gc) => rerere_gc(&git_dir),
    }
}

fn setup_rerere_options(args: &[String]) -> Result<RerereOptions> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return rerere_usage_stdout();
    }
    let parsed = match parse_options(args, rerere_option_specs(), RERERE_USAGE) {
        Ok(parsed) => parsed,
        Err(error) => {
            // git prints the `error: unknown option ...` line before the usage.
            if let Some(message) = error.message() {
                if let Some(option) = message
                    .strip_prefix("unknown option `")
                    .and_then(|rest| rest.strip_suffix('\''))
                {
                    eprintln!("error: unknown option `{option}'");
                } else if let Some(option) = message
                    .strip_prefix("unknown switch `")
                    .and_then(|rest| rest.strip_suffix('\''))
                {
                    eprintln!("error: unknown switch `{option}'");
                } else {
                    eprintln!("error: {message}");
                }
            }
            return rerere_usage();
        }
    };
    let mut autoupdate = None;
    for option in &parsed.options {
        if option.long == Some("rerere-autoupdate")
            && let sley_options::ParsedValue::Bool(value) = option.value
        {
            autoupdate = Some(value);
        }
    }
    let mut subcommand = None;
    let mut paths = Vec::new();
    for arg in &parsed.positionals {
        match *arg {
            "clear" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Clear),
            "diff" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Diff),
            "forget" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Forget),
            "gc" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Gc),
            "remaining" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Remaining),
            "status" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Status),
            _ if subcommand.is_none() => return rerere_usage(),
            value => paths.push(value.to_string()),
        }
    }
    if matches!(subcommand, Some(RerereSubcommand::Forget)) && paths.is_empty() {
        eprintln!("warning: 'git rerere forget' without paths is deprecated");
    }
    Ok(RerereOptions {
        subcommand,
        autoupdate,
        paths,
    })
}

fn rerere_usage<T>() -> Result<T> {
    eprintln!("usage: git rerere [clear | forget <pathspec>... | diff | status | remaining | gc]");
    eprintln!();
    eprintln!("    --[no-]rerere-autoupdate");
    eprintln!("                          register clean resolutions in index");
    eprintln!();
    Err(GitError::Exit(129))
}

fn rerere_usage_stdout<T>() -> Result<T> {
    println!("usage: git rerere [clear | forget <pathspec>... | diff | status | remaining | gc]");
    println!();
    println!("    --[no-]rerere-autoupdate");
    println!("                          register clean resolutions in index");
    println!();
    Err(GitError::Exit(129))
}

pub(crate) fn is_rerere_enabled(git_dir: &Path) -> bool {
    let config = rerere_effective_config(git_dir);
    sley_diff_merge::is_rerere_enabled_with_config(git_dir, &config)
}

fn rerere_effective_config(git_dir: &Path) -> GitConfig {
    let config = read_repo_config(git_dir).unwrap_or_default();
    commands::merge_rebase::effective_config_with_overrides(&config)
}

/// The stage hook behind `rerere.autoupdate`: resolve `path` into the index
/// from its worktree copy (git's add_file_to_index contract).
fn stage_resolved_path(
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    path: &str,
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let full = worktree_root.join(path);
    let content = fs::read(&full)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let oid = db.write_object(EncodedObject::new(ObjectType::Blob, content))?;
    let mode = resolved_worktree_mode(&full)?;
    let mut entries: Vec<IndexEntry> = index
        .entries
        .into_iter()
        .filter(|entry| entry.path.as_bytes() != path.as_bytes())
        .collect();
    let mut staged = commands::merge_rebase::merge_index_entry(path.as_bytes(), mode, oid, 0);
    // git's update_paths stages via add_file_to_index, which records the
    // file's stat (fill_stat_cache_info); a zeroed stat would make diff-files
    // report the freshly staged path as modified.
    if let Ok(metadata) = fs::metadata(&full) {
        sley_worktree::fill_index_entry_stat_cache(&mut staged, &metadata);
    }
    entries.push(staged);
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.stage().as_u16().cmp(&right.stage().as_u16()))
    });
    index.entries = entries;
    fs::write(index_path, index.write(format)?)?;
    Ok(())
}

#[cfg(unix)]
fn resolved_worktree_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    Ok(if mode & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    })
}

#[cfg(not(unix))]
fn resolved_worktree_mode(_path: &Path) -> Result<u32> {
    Ok(0o100644)
}

pub(crate) fn repo_rerere(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    autoupdate_override: Option<bool>,
) -> Result<bool> {
    let config = rerere_effective_config(git_dir);
    let hooks = sley_diff_merge::RerereHooks {
        stage_resolved: Some(&|git_dir, worktree_root, format, path| {
            stage_resolved_path(git_dir, format, worktree_root, path)
        }),
    };
    sley_diff_merge::repo_rerere(
        git_dir,
        worktree_root,
        format,
        &config,
        autoupdate_override,
        hooks,
        &mut sley_diff_merge::StderrRerereReporter,
    )
}

pub(crate) fn record_resolved_after_commit(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let config = rerere_effective_config(git_dir);
    let hooks = sley_diff_merge::RerereHooks {
        stage_resolved: Some(&|git_dir, worktree_root, format, path| {
            stage_resolved_path(git_dir, format, worktree_root, path)
        }),
    };
    sley_diff_merge::record_resolved_after_commit(git_dir, worktree_root, format, &config, hooks)
}

fn rerere_status(git_dir: &Path) -> Result<()> {
    let config = rerere_effective_config(git_dir);
    for path in sley_diff_merge::rerere_status_paths(git_dir, &config)? {
        println!("{path}");
    }
    Ok(())
}

fn rerere_remaining(git_dir: &Path, worktree_root: &Path, format: ObjectFormat) -> Result<()> {
    let config = rerere_effective_config(git_dir);
    for path in sley_diff_merge::rerere_remaining_paths(git_dir, worktree_root, format, &config)? {
        println!("{path}");
    }
    Ok(())
}

/// Render the preimage → current-content delta for every `MERGE_RR` entry
/// (git's `rerere diff`): a hunk-header-only unified diff with the
/// `diff --git` / `index` lines stripped.
fn rerere_diff(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    lazy_fetch: bool,
) -> Result<()> {
    if !commands::rerere::is_rerere_enabled(git_dir) {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut stdout = io::stdout();
    for entry in sley_diff_merge::read_merge_rr(git_dir)? {
        let cache_dir = git_dir.join("rr-cache").join(&entry.hash);
        let preimage = rerere_cache_file_path(&cache_dir, entry.variant);
        let full = worktree_root.join(&entry.path);
        let old = match fs::read(&preimage) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let new = match fs::read(&full) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let diff_entry = sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Modified,
            path: BString::from(entry.path.as_bytes()),
            old_path: None,
            old_mode: Some(0o100644),
            new_mode: Some(0o100644),
            old_oid: None,
            new_oid: None,
        };
        let mut rendered = Vec::new();
        write_diff_patch_entry(
            &mut rendered,
            &diff_entry,
            DiffRenderOptions {
                line_indicators: sley_diff_merge::render::LineIndicators::default(),
                suppress_blank_empty: false,
                binary: false,
                anchors: &[],
                allow_textconv: false,
                db: &db,
                lazy_fetch: crate::diff_lazy_fetch(lazy_fetch),
                worktree_root: None,
                use_worktree_new: false,
                format,
                abbrev: 7,
                src_prefix: "a/",
                dst_prefix: "b/",
                context: 3,
                userdiff: None,
                funcname: None,
                colors: None,
                word_diff: None,
                no_index_contents: Some((Some(&old), Some(&new))),
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                color_moved: None,
                interhunk: 0,
                ws_ignore: sley_diff_merge::WsIgnore::default(),
                diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
                ignore_blank_lines: false,
                ignore_regexes: &[],
                line_ranges: None,
                indent_heuristic: true,
                big_file_threshold: crate::diff_big_file_threshold(&db),
                submodule_render: crate::cli_submodule_render(),
            },
        )?;
        stdout.write_all(&rerere_diff_payload(&rendered))?;
    }
    Ok(())
}

fn rerere_cache_file_path(cache_dir: &Path, variant: u32) -> PathBuf {
    if variant == 0 {
        cache_dir.join("preimage")
    } else {
        cache_dir.join(format!("preimage.{variant}"))
    }
}

fn rerere_diff_payload(rendered: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rendered.len());
    for line in split_lines(rendered) {
        if line.starts_with(b"diff --git ") || line.starts_with(b"index ") {
            continue;
        }
        if line.starts_with(b"@@ ")
            && let Some(end) = second_hunk_marker_end(line)
        {
            out.extend_from_slice(&line[..end]);
            out.push(b'\n');
            continue;
        }
        out.extend_from_slice(line);
    }
    out
}

fn second_hunk_marker_end(line: &[u8]) -> Option<usize> {
    let mut pos = 2;
    while pos + 1 < line.len() {
        if line[pos] == b'@' && line[pos + 1] == b'@' {
            return Some(pos + 2);
        }
        pos += 1;
    }
    None
}

fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, byte) in content.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(&content[start..=idx]);
            start = idx + 1;
        }
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

pub(crate) fn rerere_clear(git_dir: &Path) -> Result<()> {
    let config = rerere_effective_config(git_dir);
    sley_diff_merge::rerere_clear(git_dir, &config)
}

fn rerere_gc(git_dir: &Path) -> Result<()> {
    let config = rerere_effective_config(git_dir);
    sley_diff_merge::rerere_gc(git_dir, &config)
}

fn rerere_forget(
    git_dir: &Path,
    paths: &[String],
    reporter: &mut dyn sley_diff_merge::RerereReporter,
) -> Result<()> {
    let config = rerere_effective_config(git_dir);
    sley_diff_merge::rerere_forget(git_dir, &config, paths, reporter)
}
