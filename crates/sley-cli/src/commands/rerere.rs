//! Native `git rerere` support.
#![allow(clippy::expect_used)]

use crate::commands::cli_options::opt_bool;
use crate::*;
use sley::plumbing::{sley_core, sley_diff_merge, sley_index, sley_worktree};
use sley_options::{OptionSpec, parse_options};
use std::time::{Duration, SystemTime};

const RERERE_MARKER_SIZE: usize = 7;
const RERERE_RESOLVED_DAYS: u64 = 60;
const RERERE_UNRESOLVED_DAYS: u64 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeRrEntry {
    hash: String,
    variant: u32,
    path: String,
}

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

pub(crate) fn cmd_rerere(args: &[String]) -> Result<()> {
    let options = setup_rerere_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    match options.subcommand {
        None => repo_rerere(&git_dir, format, options.autoupdate).map(|_| ()),
        Some(RerereSubcommand::Status) => rerere_status(&git_dir),
        Some(RerereSubcommand::Remaining) => rerere_remaining(&git_dir, format),
        Some(RerereSubcommand::Diff) => rerere_diff(&git_dir, format),
        Some(RerereSubcommand::Clear) => rerere_clear(&git_dir),
        Some(RerereSubcommand::Forget) => rerere_forget(&git_dir, &options.paths),
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
        if option.long == Some("rerere-autoupdate") {
            if let sley_options::ParsedValue::Bool(value) = option.value {
                autoupdate = Some(value);
            }
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
    if let Some(config) = commands::merge_rebase::effective_config_with_overrides()
        && let Some(value) = config.get("rerere", None, "enabled")
    {
        return commands::merge_rebase::parse_maybe_bool(value.trim()).unwrap_or(false);
    }
    git_dir.join("rr-cache").is_dir()
}

fn rerere_autoupdate(git_dir: &Path, override_value: Option<bool>) -> bool {
    if let Some(value) = override_value {
        return value;
    }
    commands::merge_rebase::effective_config_with_overrides()
        .and_then(|config| {
            config
                .get("rerere", None, "autoupdate")
                .and_then(|value| commands::merge_rebase::parse_maybe_bool(value.trim()))
        })
        .or_else(|| {
            read_repo_config(git_dir).ok().and_then(|config| {
                config
                    .get("rerere", None, "autoupdate")
                    .and_then(|value| commands::merge_rebase::parse_maybe_bool(value.trim()))
            })
        })
        .unwrap_or(false)
}

pub(crate) fn repo_rerere(
    git_dir: &Path,
    format: ObjectFormat,
    autoupdate_override: Option<bool>,
) -> Result<bool> {
    if !is_rerere_enabled(git_dir) {
        return Ok(false);
    }
    fs::create_dir_all(git_dir.join("rr-cache"))?;
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let mut rr = read_merge_rr(git_dir)?;
    let mut changed = false;
    let conflicts = find_rerere_conflicts(git_dir, format)?;
    for conflict in &conflicts {
        let full = worktree_root.join(bytes_to_path_string(&conflict.path)?);
        let Ok(content) = fs::read(&full) else {
            continue;
        };
        let (normalized, hash) = match scan_conflicted_content(&content, true)? {
            ConflictScan::Conflicted { normalized, hash } => {
                (normalized, hash.expect("hash requested"))
            }
            ConflictScan::Clean => {
                if let Some(pos) = rr
                    .iter()
                    .position(|entry| entry.path.as_bytes() == conflict.path)
                    && handle_resolved_path(git_dir, &mut rr[pos], &full, true)?
                {
                    changed = true;
                }
                continue;
            }
            ConflictScan::Malformed => continue,
        };
        let hash_hex = hash.to_hex();
        let entry_pos = if let Some(pos) = rr
            .iter()
            .position(|entry| entry.path.as_bytes() == conflict.path)
        {
            if rr[pos].hash != hash_hex {
                rr[pos].hash = hash_hex.clone();
                rr[pos].variant = 0;
                changed = true;
            }
            pos
        } else {
            rr.push(MergeRrEntry {
                hash: hash_hex.clone(),
                variant: 0,
                path: bytes_to_path_string(&conflict.path)?,
            });
            changed = true;
            rr.len() - 1
        };
        let mut entry = rr[entry_pos].clone();
        fs::create_dir_all(git_dir.join("rr-cache").join(&entry.hash))?;
        if do_rerere_one_path(
            git_dir,
            format,
            &worktree_root,
            &mut entry,
            &normalized,
            autoupdate_override,
        )? {
            rr[entry_pos] = entry;
            changed = true;
        }
    }

    for entry in &mut rr {
        if conflicts
            .iter()
            .any(|conflict| conflict.path == entry.path.as_bytes())
        {
            continue;
        }
        let full = worktree_root.join(&entry.path);
        if !full.is_file() {
            continue;
        }
        if handle_resolved_path(git_dir, entry, &full, false)? {
            changed = true;
        }
    }
    rr.retain(|entry| entry.variant != u32::MAX);
    if changed || !rr.is_empty() || git_dir.join("MERGE_RR").is_file() {
        write_merge_rr(git_dir, &rr)?;
    }
    Ok(changed)
}

fn do_rerere_one_path(
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    entry: &mut MergeRrEntry,
    normalized: &[u8],
    autoupdate_override: Option<bool>,
) -> Result<bool> {
    let rr_cache = git_dir.join("rr-cache");
    let cache_dir = rr_cache.join(&entry.hash);
    scan_variant_status(&cache_dir, entry.variant)?;
    if try_replay_resolution(git_dir, format, worktree_root, entry)? {
        if rerere_autoupdate(git_dir, autoupdate_override) {
            stage_resolved_path(git_dir, format, worktree_root, &entry.path)?;
            eprintln!("Staged '{}' using previous resolution.", entry.path);
            entry.variant = u32::MAX;
        } else {
            eprintln!("Resolved '{}' using previous resolution.", entry.path);
        }
        return Ok(true);
    }
    assign_variant_for_preimage(&cache_dir, entry, normalized)?;
    let preimage = rerere_cache_file_path(&cache_dir, entry.variant, "preimage");
    if !preimage.is_file() || fs::read(&preimage).ok().as_deref() != Some(normalized) {
        fs::write(&preimage, normalized)?;
        let postimage = rerere_cache_file_path(&cache_dir, entry.variant, "postimage");
        if postimage.is_file() {
            fs::remove_file(postimage)?;
        }
        eprintln!("Recorded preimage for '{}'", entry.path);
        return Ok(true);
    }
    Ok(false)
}

fn assign_variant_for_preimage(
    cache_dir: &Path,
    entry: &mut MergeRrEntry,
    normalized: &[u8],
) -> Result<()> {
    let mut variant = 0;
    loop {
        let preimage = rerere_cache_file_path(cache_dir, variant, "preimage");
        if !preimage.is_file() {
            entry.variant = variant;
            return Ok(());
        }
        if fs::read(&preimage).ok().as_deref() == Some(normalized) {
            entry.variant = variant;
            return Ok(());
        }
        variant += 1;
    }
}

fn scan_variant_status(cache_dir: &Path, variant: u32) -> Result<()> {
    fs::create_dir_all(cache_dir)?;
    let preimage = rerere_cache_file_path(cache_dir, variant, "preimage");
    if preimage.is_file() {
        return Ok(());
    }
    Ok(())
}

fn handle_resolved_path(
    git_dir: &Path,
    entry: &mut MergeRrEntry,
    full: &Path,
    keep_active: bool,
) -> Result<bool> {
    let content = fs::read(full)?;
    match scan_conflicted_content(&content, false)? {
        ConflictScan::Clean => {}
        ConflictScan::Malformed | ConflictScan::Conflicted { .. } => return Ok(false),
    }
    let cache_dir = git_dir.join("rr-cache").join(&entry.hash);
    let preimage = rerere_cache_file_path(&cache_dir, entry.variant, "preimage");
    if !preimage.is_file() {
        return Ok(false);
    }
    let postimage = rerere_cache_file_path(&cache_dir, entry.variant, "postimage");
    if !postimage.is_file() {
        fs::write(&postimage, &content)?;
        eprintln!("Recorded resolution for '{}'.", entry.path);
    }
    if keep_active {
        return Ok(false);
    }
    entry.variant = u32::MAX;
    Ok(true)
}

pub(crate) fn record_resolved_after_commit(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    if !is_rerere_enabled(git_dir) || !git_dir.join("MERGE_RR").is_file() {
        return Ok(());
    }
    let _ = repo_rerere(git_dir, format, None)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct RerereConflict {
    path: Vec<u8>,
}

fn find_rerere_conflicts(git_dir: &Path, format: ObjectFormat) -> Result<Vec<RerereConflict>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    let Ok(bytes) = fs::read(&index_path) else {
        return Ok(Vec::new());
    };
    let index = Index::parse(&bytes, format)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < index.entries.len() {
        let path = index.entries[i].path.clone();
        let mut has_ours = false;
        let mut has_theirs = false;
        while i < index.entries.len() && index.entries[i].path == path {
            let entry = &index.entries[i];
            let stage = entry.stage().as_u16();
            let regular = entry.mode & sley_index::GIT_MODE_TYPE_MASK == 0o100000;
            if regular && stage == 2 {
                has_ours = true;
            } else if regular && stage == 3 {
                has_theirs = true;
            }
            i += 1;
        }
        if has_ours && has_theirs {
            out.push(RerereConflict {
                path: path.as_bytes().to_vec(),
            });
        }
    }
    Ok(out)
}

fn read_merge_rr(git_dir: &Path) -> Result<Vec<MergeRrEntry>> {
    let path = git_dir.join("MERGE_RR");
    let Ok(data) = fs::read(&path) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for record in data
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(GitError::Command("corrupt MERGE_RR".into()));
        };
        let id = std::str::from_utf8(&record[..tab])
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        let (hash, variant) = parse_merge_rr_id(id)?;
        entries.push(MergeRrEntry {
            hash,
            variant,
            path: path.to_string(),
        });
    }
    Ok(entries)
}

fn write_merge_rr(git_dir: &Path, entries: &[MergeRrEntry]) -> Result<()> {
    let path = git_dir.join("MERGE_RR");
    if entries.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }
    let mut sorted = entries.to_vec();
    sorted.sort_by(|left, right| left.path.cmp(&right.path));
    let mut data = Vec::new();
    for entry in sorted {
        data.extend_from_slice(entry.hash.as_bytes());
        if entry.variant > 0 {
            data.push(b'.');
            data.extend_from_slice(entry.variant.to_string().as_bytes());
        }
        data.push(b'\t');
        data.extend_from_slice(entry.path.as_bytes());
        data.push(0);
    }
    fs::write(path, data)?;
    Ok(())
}

fn parse_merge_rr_id(id: &str) -> Result<(String, u32)> {
    if let Some(dot) = id.find('.') {
        let hash = &id[..dot];
        let variant = id[dot + 1..]
            .parse::<u32>()
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        Ok((hash.to_string(), variant))
    } else {
        Ok((id.to_string(), 0))
    }
}

fn rerere_cache_file_path(cache_dir: &Path, variant: u32, name: &str) -> PathBuf {
    if variant == 0 {
        cache_dir.join(name)
    } else {
        cache_dir.join(format!("{name}.{variant}"))
    }
}

enum ConflictScan {
    Clean,
    Malformed,
    Conflicted {
        normalized: Vec<u8>,
        hash: Option<ObjectId>,
    },
}

fn scan_conflicted_content(content: &[u8], compute_hash: bool) -> Result<ConflictScan> {
    let lines = split_lines(content);
    let mut out = Vec::with_capacity(content.len());
    let mut hash_input = Vec::new();
    let mut i = 0;
    let mut found = false;
    while i < lines.len() {
        if is_cmarker(lines[i], b'<') {
            let Some((normalized, hash_part, next)) = parse_conflict(&lines, i + 1)? else {
                return Ok(ConflictScan::Malformed);
            };
            out.extend_from_slice(&normalized);
            if compute_hash {
                hash_input.extend_from_slice(&hash_part);
            }
            found = true;
            i = next;
        } else {
            out.extend_from_slice(lines[i]);
            i += 1;
        }
    }
    if !found {
        return Ok(ConflictScan::Clean);
    }
    let hash = compute_hash
        .then(|| sley_core::digest_bytes(ObjectFormat::Sha1, &hash_input))
        .transpose()?;
    Ok(ConflictScan::Conflicted {
        normalized: out,
        hash,
    })
}

fn normalize_conflicted_content(
    content: &[u8],
    compute_hash: bool,
) -> Result<Option<(Vec<u8>, Option<ObjectId>)>> {
    match scan_conflicted_content(content, compute_hash)? {
        ConflictScan::Clean | ConflictScan::Malformed => Ok(None),
        ConflictScan::Conflicted { normalized, hash } => Ok(Some((normalized, hash))),
    }
}

type ParsedConflict = (Vec<u8>, Vec<u8>, usize);

fn parse_conflict(lines: &[&[u8]], mut i: usize) -> Result<Option<ParsedConflict>> {
    let mut one = Vec::new();
    let mut two = Vec::new();
    let mut side = 1u8;
    while i < lines.len() {
        let line = lines[i];
        if is_cmarker(line, b'<') {
            let Some((nested, _, next)) = parse_conflict(lines, i + 1)? else {
                return Ok(None);
            };
            if side == 1 {
                one.extend_from_slice(&nested);
            } else {
                two.extend_from_slice(&nested);
            }
            i = next;
            continue;
        }
        if is_cmarker(line, b'|') {
            if side != 1 {
                return Ok(None);
            }
            side = 0;
            i += 1;
            continue;
        }
        if is_cmarker(line, b'=') {
            if side > 1 {
                return Ok(None);
            }
            side = 2;
            i += 1;
            continue;
        }
        if is_cmarker(line, b'>') {
            if side != 2 {
                return Ok(None);
            }
            let (out, hash_part) = normalize_conflict_sides(&one, &two);
            return Ok(Some((out, hash_part, i + 1)));
        }
        if side == 1 {
            one.extend_from_slice(line);
        } else if side == 2 {
            two.extend_from_slice(line);
        }
        i += 1;
    }
    Ok(None)
}

fn normalize_conflict_sides(one: &[u8], two: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let one_lines = split_lines(one);
    let two_lines = split_lines(two);
    let matches = lcs_line_matches(&one_lines, &two_lines);
    let common_runs = significant_common_runs(&matches, 2);
    let mut out = Vec::new();
    let mut hash_part = Vec::new();
    let mut one_pos = 0usize;
    let mut two_pos = 0usize;
    for (match_one, match_two, len) in common_runs {
        emit_conflict_region(
            &one_lines[one_pos..match_one],
            &two_lines[two_pos..match_two],
            &mut out,
            &mut hash_part,
        );
        for line in &one_lines[match_one..match_one + len] {
            out.extend_from_slice(line);
        }
        one_pos = match_one + len;
        two_pos = match_two + len;
    }
    emit_conflict_region(
        &one_lines[one_pos..],
        &two_lines[two_pos..],
        &mut out,
        &mut hash_part,
    );
    (out, hash_part)
}

fn emit_conflict_region(
    one_lines: &[&[u8]],
    two_lines: &[&[u8]],
    out: &mut Vec<u8>,
    hash_part: &mut Vec<u8>,
) {
    if one_lines.is_empty() && two_lines.is_empty() {
        return;
    }
    let one = concat_lines(one_lines);
    let two = concat_lines(two_lines);
    let (first, second) = if one > two { (two, one) } else { (one, two) };
    write_marker(out, b'<');
    out.extend_from_slice(&first);
    write_marker(out, b'=');
    out.extend_from_slice(&second);
    write_marker(out, b'>');
    hash_part.extend_from_slice(&first);
    hash_part.push(0);
    hash_part.extend_from_slice(&second);
    hash_part.push(0);
}

fn concat_lines(lines: &[&[u8]]) -> Vec<u8> {
    let len = lines.iter().map(|line| line.len()).sum();
    let mut out = Vec::with_capacity(len);
    for line in lines {
        out.extend_from_slice(line);
    }
    out
}

fn lcs_line_matches(left: &[&[u8]], right: &[&[u8]]) -> Vec<(usize, usize)> {
    let rows = left.len();
    let cols = right.len();
    let mut dp = vec![vec![0usize; cols + 1]; rows + 1];
    for i in (0..rows).rev() {
        for j in (0..cols).rev() {
            dp[i][j] = if left[i] == right[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < rows && j < cols {
        if left[i] == right[j] {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn significant_common_runs(
    matches: &[(usize, usize)],
    min_len: usize,
) -> Vec<(usize, usize, usize)> {
    let mut runs = Vec::new();
    let mut idx = 0;
    while idx < matches.len() {
        let (start_left, start_right) = matches[idx];
        let mut len = 1;
        while idx + len < matches.len()
            && matches[idx + len].0 == start_left + len
            && matches[idx + len].1 == start_right + len
        {
            len += 1;
        }
        if len >= min_len {
            runs.push((start_left, start_right, len));
        }
        idx += len;
    }
    runs
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

fn is_cmarker(line: &[u8], marker: u8) -> bool {
    if line.len() < RERERE_MARKER_SIZE + 1 {
        return false;
    }
    if !line[..RERERE_MARKER_SIZE]
        .iter()
        .all(|byte| *byte == marker)
    {
        return false;
    }
    let next = line[RERERE_MARKER_SIZE];
    if (marker == b'<' || marker == b'>') && next != b' ' {
        return false;
    }
    next.is_ascii_whitespace()
}

fn write_marker(out: &mut Vec<u8>, marker: u8) {
    out.extend(std::iter::repeat_n(marker, RERERE_MARKER_SIZE));
    out.push(b'\n');
}

fn try_replay_resolution(
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    entry: &mut MergeRrEntry,
) -> Result<bool> {
    let cache_dir = git_dir.join("rr-cache").join(&entry.hash);
    let mut variant = 0;
    loop {
        if !variant_exists(&cache_dir, variant) {
            return Ok(false);
        }
        let mut candidate = entry.clone();
        candidate.variant = variant;
        if try_replay_resolution_variant(git_dir, format, worktree_root, &candidate)? {
            entry.variant = variant;
            return Ok(true);
        }
        variant += 1;
    }
}

fn variant_exists(cache_dir: &Path, variant: u32) -> bool {
    rerere_cache_file_path(cache_dir, variant, "preimage").is_file()
        || rerere_cache_file_path(cache_dir, variant, "postimage").is_file()
}

fn try_replay_resolution_variant(
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    entry: &MergeRrEntry,
) -> Result<bool> {
    let cache_dir = git_dir.join("rr-cache").join(&entry.hash);
    let preimage = rerere_cache_file_path(&cache_dir, entry.variant, "preimage");
    let postimage = rerere_cache_file_path(&cache_dir, entry.variant, "postimage");
    if !preimage.is_file() || !postimage.is_file() {
        return Ok(false);
    }
    let full = worktree_root.join(&entry.path);
    let content = fs::read(&full)?;
    let Some((thisimage, _)) = normalize_conflicted_content(&content, false)? else {
        return Ok(false);
    };
    let base = fs::read(&preimage)?;
    let resolved = fs::read(&postimage)?;
    let merged = sley_diff_merge::merge_blobs(
        &base,
        &thisimage,
        &resolved,
        &sley_diff_merge::MergeBlobOptions {
            ours_label: "",
            theirs_label: "",
            base_label: "",
            style: sley_diff_merge::ConflictStyle::Merge,
            favor: sley_diff_merge::MergeFavor::None,
            ws_ignore: sley_diff_merge::WsIgnore::EMPTY,
            marker_size: 7,
        },
    );
    if merged.conflicted {
        return Ok(false);
    }
    fs::write(&full, &merged.content)?;
    let _ = fs::write(
        rerere_cache_file_path(&cache_dir, entry.variant, "thisimage"),
        &thisimage,
    );
    let _ = fs::write(&postimage, &resolved);
    let _ = format;
    Ok(true)
}

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

fn rerere_status(git_dir: &Path) -> Result<()> {
    if !is_rerere_enabled(git_dir) {
        return Ok(());
    }
    for entry in read_merge_rr(git_dir)? {
        println!("{}", entry.path);
    }
    Ok(())
}

fn rerere_remaining(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    if !is_rerere_enabled(git_dir) {
        return Ok(());
    }
    let conflicts = find_rerere_conflicts(git_dir, format)?;
    let rr = read_merge_rr(git_dir)?;
    for entry in rr {
        if conflicts
            .iter()
            .any(|conflict| conflict.path == entry.path.as_bytes())
            && (!entry_has_postimage(git_dir, &entry)
                || worktree_path_still_has_conflicts(git_dir, &entry.path)?)
        {
            println!("{}", entry.path);
        }
    }
    Ok(())
}

fn entry_has_postimage(git_dir: &Path, entry: &MergeRrEntry) -> bool {
    rerere_cache_file_path(
        &git_dir.join("rr-cache").join(&entry.hash),
        entry.variant,
        "postimage",
    )
    .is_file()
}

fn worktree_path_still_has_conflicts(git_dir: &Path, path: &str) -> Result<bool> {
    let full = worktree_root_for_git_dir(git_dir)?.join(path);
    let Ok(content) = fs::read(full) else {
        return Ok(false);
    };
    Ok(!matches!(
        scan_conflicted_content(&content, false)?,
        ConflictScan::Clean
    ))
}

fn rerere_diff(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    if !is_rerere_enabled(git_dir) {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut stdout = io::stdout();
    for entry in read_merge_rr(git_dir)? {
        let cache_dir = git_dir.join("rr-cache").join(&entry.hash);
        let preimage = rerere_cache_file_path(&cache_dir, entry.variant, "preimage");
        let full = worktree_root_for_git_dir(git_dir)?.join(&entry.path);
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
            },
        )?;
        stdout.write_all(&rerere_diff_payload(&rendered))?;
    }
    Ok(())
}

fn rerere_diff_payload(rendered: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rendered.len());
    for line in split_lines(rendered) {
        if line.starts_with(b"diff --git ") || line.starts_with(b"index ") {
            continue;
        }
        if line.starts_with(b"@@ ") {
            if let Some(end) = second_hunk_marker_end(line) {
                out.extend_from_slice(&line[..end]);
                out.push(b'\n');
                continue;
            }
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

pub(crate) fn rerere_clear(git_dir: &Path) -> Result<()> {
    if !is_rerere_enabled(git_dir) {
        return Ok(());
    }
    let rr_cache = git_dir.join("rr-cache");
    for entry in read_merge_rr(git_dir)? {
        let cache_dir = rr_cache.join(&entry.hash);
        let postimage = rerere_cache_file_path(&cache_dir, entry.variant, "postimage");
        if !postimage.is_file() {
            remove_variant(&rr_cache, &entry)?;
        }
    }
    let merge_rr = git_dir.join("MERGE_RR");
    if merge_rr.is_file() {
        fs::remove_file(merge_rr)?;
    }
    Ok(())
}

fn rerere_gc(git_dir: &Path) -> Result<()> {
    let rr_cache = git_dir.join("rr-cache");
    if !rr_cache.exists() {
        return Ok(());
    }
    let resolved_expiry = rerere_expiry("rerereresolved", RERERE_RESOLVED_DAYS);
    let unresolved_expiry = rerere_expiry("rerereunresolved", RERERE_UNRESOLVED_DAYS);
    for entry in fs::read_dir(&rr_cache)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let has_postimage = find_rr_file(&path, "postimage")?.is_some();
        let expiry = if has_postimage {
            resolved_expiry
        } else {
            unresolved_expiry
        };
        if rr_dir_is_expired(&path, expiry)? {
            remove_dir_all_if_exists(&path)?;
        }
    }
    Ok(())
}

fn rerere_expiry(key: &str, days: u64) -> Duration {
    if let Some(config) = commands::merge_rebase::effective_config_with_overrides()
        && let Some(value) = config.get("gc", None, key)
    {
        return parse_expiry_duration(value).unwrap_or(Duration::from_secs(days * 86400));
    }
    Duration::from_secs(days * 86400)
}

fn parse_expiry_duration(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("now") {
        return Some(Duration::from_secs(0));
    }
    if let Some(days) = trimmed.strip_suffix(".days.ago") {
        return days
            .parse::<u64>()
            .ok()
            .map(|days| Duration::from_secs(days * 86400));
    }
    trimmed
        .parse::<u64>()
        .ok()
        .map(|days| Duration::from_secs(days * 86400))
}

fn rr_dir_is_expired(path: &Path, expiry: Duration) -> Result<bool> {
    let now = SystemTime::now();
    let mut newest = None;
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if !path.is_file() || !is_pre_or_postimage_file(&path) {
            continue;
        }
        let modified = fs::metadata(&path)?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        newest = Some(newest.map_or(modified, |current: SystemTime| current.max(modified)));
    }
    let Some(newest) = newest else {
        return Ok(true);
    };
    Ok(now.duration_since(newest).unwrap_or_default() > expiry)
}

fn is_pre_or_postimage_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == "preimage"
                || name == "postimage"
                || name.starts_with("preimage.")
                || name.starts_with("postimage.")
        })
}

fn find_rr_file(path: &Path, name: &str) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|file| file == name || file.starts_with(&format!("{name}.")))
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn rerere_path_matches(path: &str, pattern: &str) -> bool {
    path == pattern || path.ends_with(&format!("/{pattern}"))
}

fn rerere_forget(git_dir: &Path, paths: &[String]) -> Result<()> {
    if !is_rerere_enabled(git_dir) || paths.is_empty() {
        return Ok(());
    }
    let rr_cache = git_dir.join("rr-cache");
    let entries = read_merge_rr(git_dir)?;
    let mut forgotten = Vec::new();
    for pattern in paths {
        let mut matched = false;
        for entry in entries
            .iter()
            .filter(|entry| rerere_path_matches(&entry.path, pattern))
        {
            matched = true;
            let cache_dir = rr_cache.join(&entry.hash);
            let postimage = rerere_cache_file_path(&cache_dir, entry.variant, "postimage");
            if !postimage.is_file() {
                eprintln!("error: no remembered resolution for '{pattern}'");
                continue;
            }
            fs::remove_file(&postimage)?;
            forgotten.push(entry.clone());
            if let Ok(thisimage) = fs::read(rerere_cache_file_path(
                &cache_dir,
                entry.variant,
                "thisimage",
            )) {
                fs::write(
                    rerere_cache_file_path(&cache_dir, entry.variant, "preimage"),
                    thisimage,
                )?;
                eprintln!("Updated preimage for '{pattern}'");
            }
            eprintln!("Forgot resolution for '{pattern}'");
        }
        if !matched {
            eprintln!("error: no remembered resolution for '{pattern}'");
        }
    }
    if !forgotten.is_empty() {
        write_merge_rr(git_dir, &forgotten)?;
    }
    Ok(())
}

fn remove_variant(rr_cache: &Path, entry: &MergeRrEntry) -> Result<()> {
    let cache_dir = rr_cache.join(&entry.hash);
    if !cache_dir.is_dir() {
        return Ok(());
    }
    for name in ["preimage", "postimage", "thisimage"] {
        let path = rerere_cache_file_path(&cache_dir, entry.variant, name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    match fs::remove_dir(&cache_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn bytes_to_path_string(path: &[u8]) -> Result<String> {
    std::str::from_utf8(path)
        .map(str::to_string)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))
}
