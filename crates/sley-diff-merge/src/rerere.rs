//! `git rerere` engine — reuse recorded resolution (upstream colocates rerere.c
//! with the merge machinery, so this lives beside the blob/tree merge engines).
//!
//! The engine owns every byte-level concern: the `MERGE_RR` index format
//! (`<hash>[.<variant>]\t<path>\0` records), the `rr-cache/<conflict-id>`
//! preimage/postimage/thisimage files, conflict-id computation from merge
//! conflict markers (marker-size-7 normalization plus LCS-based side sorting),
//! remember/forget/reuse logic, and expiry GC. Porcelain supplies the effective
//! configuration view and two seams: a reporter for user-facing progress lines
//! and a stage hook that resolves a path into the index once autoupdate
//! replays it clean.
//!
//! Everything here is deterministic given the repository state; the CLI command
//! is an argv/rendering shell over these entry points.

use crate::{MergeBlobOptions, MergeFavor, WsIgnore, merge_blobs};
use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_index::Index;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const RERERE_MARKER_SIZE: usize = 7;
const RERERE_RESOLVED_DAYS: u64 = 60;
const RERERE_UNRESOLVED_DAYS: u64 = 15;

/// Sink for rerere's progress messages ("Recorded preimage for '...'",
/// "Resolved '...' using previous resolution.", ...). The engine emits fully
/// formatted lines; porcelain decides where they go. git prints them on stderr.
pub trait RerereReporter {
    fn report(&mut self, message: &str);
}

/// Default reporter: stderr, one line per message (git's behaviour).
#[derive(Default)]
pub struct StderrRerereReporter;

impl RerereReporter for StderrRerereReporter {
    fn report(&mut self, message: &str) {
        eprintln!("{message}");
    }
}

/// Porcelain-provided staging seam. When `rerere.autoupdate` replays a recorded
/// resolution cleanly, git stages the resolved path via `add_file_to_index`;
/// the engine delegates that index mutation to this hook.
pub type RerereStageHook<'a> = &'a dyn Fn(&Path, &Path, ObjectFormat, &str) -> Result<()>;

/// Porcelain seams for [`repo_rerere`].
#[derive(Clone, Copy, Default)]
pub struct RerereHooks<'a> {
    /// Stage a resolved path into the index (`rerere.autoupdate`).
    pub stage_resolved: Option<RerereStageHook<'a>>,
}

/// One record of `MERGE_RR`: the conflict id (`rr-cache` directory), the
/// variant slot within that id, and the conflicted worktree path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRrEntry {
    pub hash: String,
    pub variant: u32,
    pub path: String,
}

/// One conflicted path the rerere engine tracks (git's `rerere_path`).
#[derive(Debug, Clone)]
pub struct RerereConflict {
    pub path: Vec<u8>,
}

/// git's `rerere_enabled`: an explicit `rerere.enabled` bool wins; otherwise
/// rerere is on exactly when an `rr-cache` directory already exists.
pub fn is_rerere_enabled_with_config(git_dir: &Path, config: &GitConfig) -> bool {
    if let Some(value) = config.get("rerere", None, "enabled") {
        return parse_maybe_bool(value.trim()).unwrap_or(false);
    }
    git_dir.join("rr-cache").is_dir()
}

fn parse_maybe_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// git's `rerere.autoupdate` resolution: an explicit command-line override
/// wins; otherwise consult `rerere.autoupdate` (default off).
pub fn rerere_autoupdate_with_config(config: &GitConfig, override_value: Option<bool>) -> bool {
    if let Some(value) = override_value {
        return value;
    }
    config
        .get("rerere", None, "autoupdate")
        .and_then(|value| parse_maybe_bool(value.trim()))
        .unwrap_or(false)
}

/// Run one rerere pass over the current merge state (the `rerere()` driver in
/// `rerere.c`). Scans the index for two-sided conflicts, records preimages for
/// unseen conflict shapes, replays recorded postimages onto resolved-shaped
/// content, records resolutions for hand-resolved paths, and prunes settled
/// `MERGE_RR` entries. Returns whether any state changed.
pub fn repo_rerere(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    autoupdate_override: Option<bool>,
    hooks: RerereHooks<'_>,
    reporter: &mut dyn RerereReporter,
) -> Result<bool> {
    if !is_rerere_enabled_with_config(git_dir, config) {
        return Ok(false);
    }
    fs::create_dir_all(git_dir.join("rr-cache"))?;
    let mut rr = read_merge_rr(git_dir)?;
    let mut changed = false;
    let conflicts = find_rerere_conflicts(git_dir, format)?;
    for conflict in &conflicts {
        let full = worktree_root.join(bytes_to_path_string(&conflict.path)?);
        let Ok(content) = fs::read(&full) else {
            continue;
        };
        let (normalized, hash) = match scan_conflicted_content(&content, true)? {
            // compute_hash=true guarantees a conflict id; a missing one means
            // the content is not rerere-addressable, so skip it rather than
            // trust the invariant with a panic.
            ConflictScan::Conflicted {
                normalized,
                hash: Some(hash),
            } => (normalized, hash),
            ConflictScan::Conflicted { .. } => continue,
            ConflictScan::Clean => {
                if let Some(pos) = rr
                    .iter()
                    .position(|entry| entry.path.as_bytes() == conflict.path)
                    && handle_resolved_path(git_dir, &mut rr[pos], &full, true, reporter)?
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
            config,
            git_dir,
            format,
            worktree_root,
            &mut entry,
            &normalized,
            autoupdate_override,
            hooks,
            reporter,
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
        if handle_resolved_path(git_dir, entry, &full, false, reporter)? {
            changed = true;
        }
    }
    rr.retain(|entry| entry.variant != u32::MAX);
    if changed || !rr.is_empty() || git_dir.join("MERGE_RR").is_file() {
        write_merge_rr(git_dir, &rr)?;
    }
    Ok(changed)
}

#[allow(clippy::too_many_arguments)]
fn do_rerere_one_path(
    config: &GitConfig,
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    entry: &mut MergeRrEntry,
    normalized: &[u8],
    autoupdate_override: Option<bool>,
    hooks: RerereHooks<'_>,
    reporter: &mut dyn RerereReporter,
) -> Result<bool> {
    let rr_cache = git_dir.join("rr-cache");
    let cache_dir = rr_cache.join(&entry.hash);
    scan_variant_status(&cache_dir, entry.variant)?;
    if try_replay_resolution(git_dir, format, worktree_root, entry)? {
        if rerere_autoupdate_with_config(config, autoupdate_override) {
            if let Some(stage_resolved) = hooks.stage_resolved {
                stage_resolved(git_dir, worktree_root, format, &entry.path)?;
            }
            reporter.report(&format!(
                "Staged '{}' using previous resolution.",
                entry.path
            ));
            entry.variant = u32::MAX;
        } else {
            reporter.report(&format!(
                "Resolved '{}' using previous resolution.",
                entry.path
            ));
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
        reporter.report(&format!("Recorded preimage for '{}'", entry.path));
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
    let _preimage = rerere_cache_file_path(cache_dir, variant, "preimage");
    Ok(())
}

fn handle_resolved_path(
    git_dir: &Path,
    entry: &mut MergeRrEntry,
    full: &Path,
    keep_active: bool,
    reporter: &mut dyn RerereReporter,
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
        reporter.report(&format!("Recorded resolution for '{}'.", entry.path));
    }
    if keep_active {
        return Ok(false);
    }
    entry.variant = u32::MAX;
    Ok(true)
}

/// Post-commit/merge follow-up: record resolutions for paths the user just
/// hand-resolved and committed (git's post-commit `rerere` invocation).
pub fn record_resolved_after_commit(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    hooks: RerereHooks<'_>,
) -> Result<()> {
    if !is_rerere_enabled_with_config(git_dir, config) || !git_dir.join("MERGE_RR").is_file() {
        return Ok(());
    }
    let _ = repo_rerere(
        git_dir,
        worktree_root,
        format,
        config,
        None,
        hooks,
        &mut StderrRerereReporter,
    )?;
    Ok(())
}

/// Every path with both an ours (stage 2) and theirs (stage 3) regular-file
/// entry in the current index — the set of conflicts rerere participates in.
pub fn find_rerere_conflicts(git_dir: &Path, format: ObjectFormat) -> Result<Vec<RerereConflict>> {
    let index_path = rerere_index_path(git_dir);
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

/// git reads `$GIT_INDEX_FILE` when set (the same env seam the worktree
/// crate's `repository_index_path` honours).
fn rerere_index_path(git_dir: &Path) -> PathBuf {
    std::env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.join("index"))
}

/// Parse `MERGE_RR`: NUL-separated `<hash>[.<variant>]\t<path>` records.
pub fn read_merge_rr(git_dir: &Path) -> Result<Vec<MergeRrEntry>> {
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

/// Serialize `MERGE_RR`; an empty entry list removes the file (git's contract:
/// no pending rerere state).
pub fn write_merge_rr(git_dir: &Path, entries: &[MergeRrEntry]) -> Result<()> {
    let path = git_dir.join("MERGE_RR");
    if entries.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
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

/// Scan file content for merge conflict markers. Returns the normalized shape
/// of each conflict hunk pair (sides re-sorted over their LCS so equivalent
/// conflicts hash identically regardless of which side was ours) and — with
/// `compute_hash` — the SHA-1 conflict id git keys `rr-cache` directories on.
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

/// Normalize conflicted content to its marker-free shape; `None` when the
/// content carries no parseable conflict hunk (clean or malformed markers).
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

/// git's `handle_conflict` normalization: hoist common runs (>= 2 lines by
/// LCS) out of the two sides, then emit each remaining region with the sides
/// in sorted order so the same logical conflict always produces the same
/// preimage/hash whichever way ours/theirs were laid out.
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

/// A line whose first `marker_size` bytes are `marker`, followed by whitespace
/// (with the `<`/`>` forms additionally requiring the trailing space git writes).
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
    // Replay = three-way merge of the recorded resolution onto this occurrence
    // (git's `merge()` with the recorded preimage/postimage as base/ours).
    let merged = merge_blobs(
        &base,
        &thisimage,
        &resolved,
        &MergeBlobOptions {
            ours_label: "",
            theirs_label: "",
            base_label: "",
            style: crate::ConflictStyle::Merge,
            favor: MergeFavor::None,
            ws_ignore: WsIgnore::EMPTY,
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

/// Paths currently listed in `MERGE_RR` (git's `rerere status` payload).
pub fn rerere_status_paths(git_dir: &Path, config: &GitConfig) -> Result<Vec<String>> {
    if !is_rerere_enabled_with_config(git_dir, config) {
        return Ok(Vec::new());
    }
    Ok(read_merge_rr(git_dir)?
        .into_iter()
        .map(|entry| entry.path)
        .collect())
}

/// Paths whose conflicts remain unresolved (git's `rerere remaining` payload):
/// entries still matching a live two-sided conflict that either have no
/// recorded postimage or whose worktree copy still shows conflict markers.
pub fn rerere_remaining_paths(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<Vec<String>> {
    if !is_rerere_enabled_with_config(git_dir, config) {
        return Ok(Vec::new());
    }
    let conflicts = find_rerere_conflicts(git_dir, format)?;
    let rr = read_merge_rr(git_dir)?;
    let mut out = Vec::new();
    for entry in rr {
        if conflicts
            .iter()
            .any(|conflict| conflict.path == entry.path.as_bytes())
            && (!entry_has_postimage(git_dir, &entry)
                || worktree_path_still_has_conflicts(worktree_root, &entry.path)?)
        {
            out.push(entry.path);
        }
    }
    Ok(out)
}

fn entry_has_postimage(git_dir: &Path, entry: &MergeRrEntry) -> bool {
    rerere_cache_file_path(
        &git_dir.join("rr-cache").join(&entry.hash),
        entry.variant,
        "postimage",
    )
    .is_file()
}

fn worktree_path_still_has_conflicts(worktree_root: &Path, path: &str) -> Result<bool> {
    let full = worktree_root.join(path);
    let Ok(content) = fs::read(full) else {
        return Ok(false);
    };
    Ok(!matches!(
        scan_conflicted_content(&content, false)?,
        ConflictScan::Clean
    ))
}

/// Clear the in-progress rerere state (git's `rerere clear`): drop unresolved
/// variants referenced by `MERGE_RR` and remove the file itself.
pub fn rerere_clear(git_dir: &Path, config: &GitConfig) -> Result<()> {
    if !is_rerere_enabled_with_config(git_dir, config) {
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

/// Expire stale `rr-cache` entries (git's `rerere gc`): directories whose
/// newest pre/post image is older than `gc.rerereresolved` (default 60 days,
/// with a postimage) or `gc.rerereunresolved` (default 15 days, without).
pub fn rerere_gc(git_dir: &Path, config: &GitConfig) -> Result<()> {
    let rr_cache = git_dir.join("rr-cache");
    if !rr_cache.exists() {
        return Ok(());
    }
    let resolved_expiry = rerere_expiry(config, "rerereresolved", RERERE_RESOLVED_DAYS);
    let unresolved_expiry = rerere_expiry(config, "rerereunresolved", RERERE_UNRESOLVED_DAYS);
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

fn rerere_expiry(config: &GitConfig, key: &str, days: u64) -> Duration {
    if let Some(value) = config.get("gc", None, key) {
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

/// Forget recorded resolutions (git's `rerere forget <pathspec>...`): drop the
/// postimage of every matched entry, roll any thisimage back into the
/// preimage, and rewrite `MERGE_RR` down to the forgotten set.
pub fn rerere_forget(
    git_dir: &Path,
    config: &GitConfig,
    paths: &[String],
    reporter: &mut dyn RerereReporter,
) -> Result<()> {
    if !is_rerere_enabled_with_config(git_dir, config) || paths.is_empty() {
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
                reporter.report(&format!("error: no remembered resolution for '{pattern}'"));
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
                reporter.report(&format!("Updated preimage for '{pattern}'"));
            }
            reporter.report(&format!("Forgot resolution for '{pattern}'"));
        }
        if !matched {
            reporter.report(&format!("error: no remembered resolution for '{pattern}'"));
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    match fs::remove_dir(&cache_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn bytes_to_path_string(path: &[u8]) -> Result<String> {
    sley_core::paths::bytes_to_path_string(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_scan_hashes_equivalent_orders_identically() {
        let ours_first = b"<<<<<<< \nalpha\n=======\nbeta\n>>>>>>> \ntrailing\n";
        let theirs_first = b"<<<<<<< \nbeta\n=======\nalpha\n>>>>>>> \ntrailing\n";
        let left = normalize_conflicted_content(ours_first, true)
            .expect("scan")
            .expect("conflicted");
        let right = normalize_conflicted_content(theirs_first, true)
            .expect("scan")
            .expect("conflicted");
        assert_eq!(left.1, right.1, "side order must not change conflict id");
        assert_eq!(left.0, right.0);
    }

    #[test]
    fn clean_content_scans_clean_and_malformed_markers_are_ignored() {
        assert!(matches!(
            scan_conflicted_content(b"plain\n", true).expect("clean"),
            ConflictScan::Clean
        ));
        // Unterminated hunk (no >>>>>>> line) is malformed, not fatal.
        assert!(matches!(
            scan_conflicted_content(b"<<<<<<< \nalpha\n", true).expect("malformed"),
            ConflictScan::Malformed
        ));
    }

    #[test]
    fn merge_rr_round_trips_variants_sorted_by_path() {
        let dir = tempfile::tempdir().expect("tmp");
        let git_dir = dir.path();
        write_merge_rr(
            git_dir,
            &[
                MergeRrEntry {
                    hash: "aaaa".into(),
                    variant: 2,
                    path: "z".into(),
                },
                MergeRrEntry {
                    hash: "bbbb".into(),
                    variant: 0,
                    path: "a/b".into(),
                },
            ],
        )
        .expect("write");
        let entries = read_merge_rr(git_dir).expect("read");
        assert_eq!(
            entries,
            vec![
                MergeRrEntry {
                    hash: "bbbb".into(),
                    variant: 0,
                    path: "a/b".into()
                },
                MergeRrEntry {
                    hash: "aaaa".into(),
                    variant: 2,
                    path: "z".into()
                },
            ]
        );
        write_merge_rr(git_dir, &[]).expect("clear");
        assert!(!git_dir.join("MERGE_RR").exists());
    }

    #[test]
    fn enabled_flag_follows_git_contract() {
        let dir = tempfile::tempdir().expect("tmp");
        let git_dir = dir.path();
        // No rerere.enabled, no rr-cache → off.
        assert!(!is_rerere_enabled_with_config(
            git_dir,
            &GitConfig::default()
        ));
        // rr-cache present → on even without config.
        fs::create_dir_all(git_dir.join("rr-cache")).expect("mkdir");
        assert!(is_rerere_enabled_with_config(
            git_dir,
            &GitConfig::default()
        ));
        // Explicit false wins over rr-cache presence.
        let config = GitConfig::parse(b"[rerere]\n\tenabled = false\n").expect("config");
        assert!(!is_rerere_enabled_with_config(git_dir, &config));
    }
}
