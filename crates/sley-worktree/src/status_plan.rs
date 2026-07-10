//! Reusable planning for porcelain status.
//!
//! This layer turns the raw index/worktree scan into a caller-independent
//! model: path filtering and recursive-untracked collapsing, submodule ignore
//! policy, and rename/copy pairing. Rendering remains a CLI concern.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sley_config::GitConfig;
use sley_core::{ObjectFormat, ObjectId, Result};
use sley_object::{EncodedObject, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};

use crate::status::set_worktree_path_from_repo_path;
use crate::{
    ShortStatusEntry, ShortStatusOptions, StatusUntrackedMode, StreamControl,
    stream_short_status_with_database, worktree_root_for_git_dir,
};

/// What kind of rename/copy detection a status plan performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusRenameDetect {
    Off,
    Renames,
    Copies,
}

/// Resolved rename/copy settings for a status plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusRenameConfig {
    pub detect: StatusRenameDetect,
    pub rename_threshold: u8,
    pub copy_threshold: u8,
}

impl StatusRenameConfig {
    pub fn enabled(self) -> bool {
        self.detect != StatusRenameDetect::Off
    }
}

impl Default for StatusRenameConfig {
    fn default() -> Self {
        Self {
            detect: StatusRenameDetect::Renames,
            rename_threshold: 50,
            copy_threshold: 50,
        }
    }
}

fn config_rename_value(value: Option<&str>) -> i8 {
    match value {
        None => 1,
        Some(value) => {
            let lower = value.to_ascii_lowercase();
            if lower == "copies" || lower == "copy" {
                2
            } else if !matches!(lower.as_str(), "false" | "no" | "off" | "0" | "") {
                1
            } else {
                0
            }
        }
    }
}

fn parse_rename_score_percent(arg: &str) -> u8 {
    let mut num: u64 = 0;
    let mut scale: u64 = 1;
    let mut dot = false;
    for ch in arg.bytes() {
        match ch {
            b'.' if !dot => {
                scale = 1;
                dot = true;
            }
            b'%' => {
                scale = if dot { scale * 100 } else { 100 };
                break;
            }
            b'0'..=b'9' => {
                if scale < 100_000 {
                    scale *= 10;
                    num = num * 10 + u64::from(ch - b'0');
                }
            }
            _ => break,
        }
    }
    if num >= scale {
        100
    } else {
        (100 * num / scale) as u8
    }
}

/// Resolve config and command-line rename settings into one engine option.
pub fn resolve_status_rename_config(
    config: &GitConfig,
    cli_no_renames: Option<bool>,
    cli_rename_score: Option<Option<String>>,
) -> StatusRenameConfig {
    let mut detect: i8 = -1;
    if let Some(value) = config.get("diff", None, "renames")
        && detect == -1
    {
        detect = config_rename_value(Some(value));
    }
    if let Some(value) = config.get("status", None, "renames") {
        detect = config_rename_value(Some(value));
    }
    let mut threshold = 50;
    if let Some(no) = cli_no_renames {
        detect = if no { 0 } else { 1 };
    }
    if let Some(score) = cli_rename_score {
        if detect < 1 {
            detect = 1;
        }
        if let Some(score) = score {
            threshold = parse_rename_score_percent(&score);
        }
    }
    StatusRenameConfig {
        detect: match detect {
            0 => StatusRenameDetect::Off,
            2 => StatusRenameDetect::Copies,
            _ => StatusRenameDetect::Renames,
        },
        rename_threshold: threshold,
        copy_threshold: threshold,
    }
}

/// One planned output row. `rename_from` is populated for rename/copy pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusOutputEntry {
    pub entry: ShortStatusEntry,
    pub rename_from: Option<Vec<u8>>,
}

/// Collection controls independent of any CLI pathspec implementation.
#[derive(Debug, Clone, Copy)]
pub struct StatusCollectionOptions<'a> {
    pub status: ShortStatusOptions,
    pub path_filter_active: bool,
    pub submodule_ignore: Option<&'a SubmoduleIgnoreResolver>,
}

/// Collected status rows before byte rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCollectionOutcome {
    pub entries: Vec<ShortStatusEntry>,
}

/// Collect and normalize status once, while sharing the repository ODB.
///
/// The two callbacks keep CLI pathspec syntax out of the engine. Embedders can
/// supply any matcher and recursive-directory projection with the same model.
pub fn collect_status_plan<F, C>(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: StatusCollectionOptions<'_>,
    mut path_matches: F,
    mut recursive_directory_for: C,
) -> Result<StatusCollectionOutcome>
where
    F: FnMut(&[u8]) -> bool,
    C: FnMut(&[u8]) -> Option<Vec<u8>>,
{
    let mut scan_options = options.status;
    if scan_options.include_ignored
        && options.path_filter_active
        && matches!(scan_options.untracked_mode, StatusUntrackedMode::Normal)
    {
        // A recursive path filter needs leaf rows before it can roll them up.
        scan_options.untracked_mode = StatusUntrackedMode::All;
    }
    let mut entries = Vec::new();
    stream_short_status_with_database(
        worktree_root,
        git_dir,
        format,
        db,
        scan_options,
        |row| {
            entries.push(row.to_owned_entry());
            Ok(StreamControl::Continue)
        },
    )?;
    if options.path_filter_active {
        entries.retain(|entry| path_matches(&entry.path));
    }
    if let Some(resolver) = options.submodule_ignore {
        apply_submodule_ignore(&mut entries, resolver);
    }
    if options.status.include_ignored
        && options.path_filter_active
        && matches!(options.status.untracked_mode, StatusUntrackedMode::Normal)
    {
        let mut collapsed = BTreeMap::new();
        for mut entry in entries {
            if entry.index == b'?'
                && entry.worktree == b'?'
                && let Some(directory) = recursive_directory_for(&entry.path)
            {
                entry.path = directory;
            }
            collapsed
                .entry((entry.index, entry.worktree, entry.path.clone()))
                .or_insert(entry);
        }
        entries = collapsed.into_values().collect();
        entries.sort_by(|left, right| {
            output_sort_category(left)
                .cmp(&output_sort_category(right))
                .then_with(|| left.path.cmp(&right.path))
        });
    }
    Ok(StatusCollectionOutcome { entries })
}

fn output_sort_category(entry: &ShortStatusEntry) -> u8 {
    match (entry.index, entry.worktree) {
        (b'?', b'?') => 1,
        (b'!', b'!') => 2,
        _ => 0,
    }
}

/// The layered submodule-ignore policy used by status and commit previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreSubmodules {
    None,
    Untracked,
    Dirty,
    All,
}

impl IgnoreSubmodules {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "untracked" => Some(Self::Untracked),
            "dirty" => Some(Self::Dirty),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Effective per-submodule policy from CLI, repository config, `.gitmodules`,
/// and `diff.ignoreSubmodules`, in precedence order.
#[derive(Debug, Clone)]
pub struct SubmoduleIgnoreResolver {
    cli: Option<IgnoreSubmodules>,
    diff_default: Option<IgnoreSubmodules>,
    by_path_repo: BTreeMap<Vec<u8>, IgnoreSubmodules>,
    by_path_gitmodules: BTreeMap<Vec<u8>, IgnoreSubmodules>,
}

impl SubmoduleIgnoreResolver {
    pub fn load(git_dir: &Path, config: &GitConfig, cli: Option<IgnoreSubmodules>) -> Result<Self> {
        let worktree = worktree_root_for_git_dir(git_dir)?;
        Ok(Self::load_for_worktree(worktree.as_deref(), config, cli))
    }

    pub fn load_for_worktree(
        worktree_root: Option<&Path>,
        config: &GitConfig,
        cli: Option<IgnoreSubmodules>,
    ) -> Self {
        let by_path_gitmodules = worktree_root
            .and_then(|root| GitConfig::read(root.join(".gitmodules")).ok())
            .map(|config| submodule_ignore_by_path(&config))
            .unwrap_or_default();
        Self {
            cli,
            diff_default: config
                .get("diff", None, "ignoreSubmodules")
                .and_then(IgnoreSubmodules::parse),
            by_path_repo: submodule_ignore_by_path(config),
            by_path_gitmodules,
        }
    }

    pub fn for_path(&self, path: &[u8]) -> IgnoreSubmodules {
        self.cli
            .or_else(|| self.by_path_repo.get(path).copied())
            .or_else(|| self.by_path_gitmodules.get(path).copied())
            .or(self.diff_default)
            .unwrap_or(IgnoreSubmodules::None)
    }

    pub fn cli_suppresses_summary(&self) -> bool {
        self.cli == Some(IgnoreSubmodules::All)
    }
}

fn submodule_ignore_by_path(config: &GitConfig) -> BTreeMap<Vec<u8>, IgnoreSubmodules> {
    let set = sley_submodule::SubmoduleConfigSet::parse(config);
    let mut map = BTreeMap::new();
    for submodule in set.iter() {
        let (Some(path), Some(ignore)) = (
            submodule.path.as_deref(),
            submodule
                .ignore
                .as_deref()
                .and_then(IgnoreSubmodules::parse),
        ) else {
            continue;
        };
        map.insert(path.as_bytes().to_vec(), ignore);
    }
    map
}

pub fn apply_submodule_ignore(
    entries: &mut Vec<ShortStatusEntry>,
    resolver: &SubmoduleIgnoreResolver,
) {
    entries.retain_mut(|entry| apply_submodule_ignore_entry(entry, resolver));
}

pub fn apply_submodule_ignore_entry(
    entry: &mut ShortStatusEntry,
    resolver: &SubmoduleIgnoreResolver,
) -> bool {
    let is_gitlink = entry.head_mode.is_some_and(sley_index::is_gitlink)
        || entry.index_mode.is_some_and(sley_index::is_gitlink)
        || entry.worktree_mode.is_some_and(sley_index::is_gitlink);
    if resolver.cli == Some(IgnoreSubmodules::All) && is_gitlink {
        return false;
    }
    let Some(submodule) = entry.submodule.as_mut() else {
        return true;
    };
    match resolver.for_path(&entry.path) {
        IgnoreSubmodules::None => {}
        IgnoreSubmodules::Untracked => submodule.untracked_content = false,
        IgnoreSubmodules::Dirty => {
            submodule.untracked_content = false;
            submodule.modified_content = false;
        }
        IgnoreSubmodules::All => {
            submodule.new_commits = false;
            submodule.modified_content = false;
            submodule.untracked_content = false;
        }
    }
    if !submodule.any() {
        entry.submodule = None;
        entry.worktree = b' ';
        return entry.index != b' ';
    }
    true
}

/// Pair status rows into exact and inexact renames/copies while sharing the
/// repository object database for similarity reads.
pub fn status_entries_with_renames(
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    entries: Vec<ShortStatusEntry>,
    rename_config: StatusRenameConfig,
) -> Result<Vec<StatusOutputEntry>> {
    if !rename_config.enabled() {
        return Ok(entries
            .into_iter()
            .map(|entry| StatusOutputEntry {
                entry,
                rename_from: None,
            })
            .collect());
    }
    let mut worktree_oids = BTreeMap::new();
    let mut output =
        status_entries_with_exact_renames(worktree_root, format, entries, &mut worktree_oids)?;
    apply_inexact_staged_renames(db, &mut output, rename_config);
    Ok(output)
}

fn status_entries_with_exact_renames(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: Vec<ShortStatusEntry>,
    worktree_oids: &mut BTreeMap<Vec<u8>, Option<ObjectId>>,
) -> Result<Vec<StatusOutputEntry>> {
    let mut used = vec![false; entries.len()];
    let mut staged_deletes = Vec::<ShortStatusEntry>::new();
    let mut staged_used = Vec::<bool>::new();
    let mut residual_deletes = Vec::<ShortStatusEntry>::new();
    let mut residual_used = Vec::<bool>::new();
    let mut output = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if used[index] {
            continue;
        }
        if entry.index == b' ' && entry.worktree == b'D' {
            let mut has_later_add = false;
            for added in entries.iter().skip(index + 1) {
                if exact_worktree_rename(worktree_root, format, entry, added, worktree_oids)? {
                    has_later_add = true;
                    break;
                }
            }
            if has_later_add {
                residual_deletes.push(entry.clone());
                residual_used.push(false);
                used[index] = true;
                continue;
            }
        }
        if entry.index == b'D' && entry.worktree == b' ' {
            let has_later_add = entries
                .iter()
                .skip(index + 1)
                .any(|added| exact_staged_rename(entry, added));
            if has_later_add {
                staged_deletes.push(entry.clone());
                staged_used.push(false);
                used[index] = true;
                continue;
            }
        }
        if entry.index == b'A' {
            let added_base = sley_diff_merge::path_basename(&entry.path);
            let mut staged_match: Option<(usize, Option<usize>, ShortStatusEntry)> = None;
            let mut chosen_same_basename = false;
            for (candidate_index, candidate) in entries.iter().enumerate() {
                if used[candidate_index] || !exact_staged_rename(candidate, entry) {
                    continue;
                }
                let same = sley_diff_merge::path_basename(&candidate.path) == added_base;
                if staged_match.is_none() || (same && !chosen_same_basename) {
                    staged_match = Some((candidate_index, None, candidate.clone()));
                    chosen_same_basename = same;
                    if same {
                        break;
                    }
                }
            }
            if !chosen_same_basename {
                for (staged_index, candidate) in staged_deletes.iter().enumerate() {
                    if staged_used[staged_index] || !exact_staged_rename(candidate, entry) {
                        continue;
                    }
                    let same = sley_diff_merge::path_basename(&candidate.path) == added_base;
                    if staged_match.is_none() || (same && !chosen_same_basename) {
                        staged_match = Some((index, Some(staged_index), candidate.clone()));
                        chosen_same_basename = same;
                        if same {
                            break;
                        }
                    }
                }
            }
            let Some((deleted_index, staged_index, deleted)) = staged_match else {
                used[index] = true;
                output.push(StatusOutputEntry {
                    entry: entry.clone(),
                    rename_from: None,
                });
                continue;
            };
            let mut renamed = entry.clone();
            renamed.index = b'R';
            renamed.worktree = b' ';
            renamed.head_mode = deleted.head_mode;
            renamed.head_oid = deleted.head_oid;
            renamed.worktree_mode = entry.index_mode;
            used[index] = true;
            if let Some(staged_index) = staged_index {
                staged_used[staged_index] = true;
            } else {
                used[deleted_index] = true;
            }
            output.push(StatusOutputEntry {
                entry: renamed,
                rename_from: Some(deleted.path.clone()),
            });
            if entry.worktree != b' ' {
                let mut residual = entry.clone();
                residual.index = b' ';
                residual.head_mode = entry.index_mode;
                residual.head_oid = entry.index_oid;
                residual_deletes.push(residual);
                residual_used.push(false);
            }
            continue;
        }
        if entry.index == b' ' && entry.worktree == b'A' {
            let mut worktree_match = None;
            for (candidate_index, candidate) in entries.iter().enumerate() {
                if used[candidate_index] {
                    continue;
                }
                if exact_worktree_rename(worktree_root, format, candidate, entry, worktree_oids)? {
                    worktree_match = Some((candidate_index, None, candidate.clone()));
                    break;
                }
            }
            if worktree_match.is_none() {
                for (residual_index, candidate) in residual_deletes.iter().enumerate() {
                    if residual_used[residual_index] {
                        continue;
                    }
                    if exact_worktree_rename(
                        worktree_root,
                        format,
                        candidate,
                        entry,
                        worktree_oids,
                    )? {
                        worktree_match = Some((index, Some(residual_index), candidate.clone()));
                        break;
                    }
                }
            }
            let Some((deleted_index, residual_index, deleted)) = worktree_match else {
                used[index] = true;
                output.push(StatusOutputEntry {
                    entry: entry.clone(),
                    rename_from: None,
                });
                continue;
            };
            let mut renamed = entry.clone();
            renamed.worktree = b'R';
            renamed.head_mode = deleted.head_mode;
            renamed.index_mode = deleted.index_mode;
            renamed.head_oid = deleted.head_oid;
            renamed.index_oid = deleted.index_oid;
            renamed.worktree_mode = entry.worktree_mode;
            used[index] = true;
            if let Some(residual_index) = residual_index {
                residual_used[residual_index] = true;
            } else {
                used[deleted_index] = true;
            }
            output.push(StatusOutputEntry {
                entry: renamed,
                rename_from: Some(deleted.path.clone()),
            });
            continue;
        }
        used[index] = true;
        output.push(StatusOutputEntry {
            entry: entry.clone(),
            rename_from: None,
        });
    }
    for (entry, used) in staged_deletes.into_iter().zip(staged_used) {
        if !used {
            output.push(StatusOutputEntry {
                entry,
                rename_from: None,
            });
        }
    }
    for (entry, used) in residual_deletes.into_iter().zip(residual_used) {
        if !used {
            output.push(StatusOutputEntry {
                entry,
                rename_from: None,
            });
        }
    }
    Ok(output)
}

fn exact_staged_rename(deleted: &ShortStatusEntry, added: &ShortStatusEntry) -> bool {
    deleted.index == b'D'
        && deleted.worktree == b' '
        && added.index == b'A'
        && deleted.head_mode == added.index_mode
        && deleted.head_oid == added.index_oid
}

fn exact_worktree_rename(
    worktree_root: &Path,
    format: ObjectFormat,
    deleted: &ShortStatusEntry,
    added: &ShortStatusEntry,
    worktree_oids: &mut BTreeMap<Vec<u8>, Option<ObjectId>>,
) -> Result<bool> {
    if deleted.index != b' '
        || deleted.worktree != b'D'
        || added.index != b' '
        || added.worktree != b'A'
        || deleted.index_mode != added.worktree_mode
    {
        return Ok(false);
    }
    let Some(index_oid) = deleted.index_oid else {
        return Ok(false);
    };
    let worktree_oid = if let Some(oid) = worktree_oids.get(&added.path) {
        *oid
    } else {
        let oid = worktree_blob_oid(worktree_root, format, &added.path)?;
        worktree_oids.insert(added.path.clone(), oid);
        oid
    };
    Ok(worktree_oid == Some(index_oid))
}

fn worktree_blob_oid(
    worktree_root: &Path,
    format: ObjectFormat,
    path: &[u8],
) -> Result<Option<ObjectId>> {
    let mut absolute = worktree_root.to_path_buf();
    set_worktree_path_from_repo_path(worktree_root, path, &mut absolute)?;
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    if metadata.is_dir() {
        return Ok(None);
    }
    let body = if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            fs::read_link(&absolute)?.as_os_str().as_bytes().to_vec()
        }
        #[cfg(not(unix))]
        {
            fs::read_link(&absolute)?
                .to_string_lossy()
                .replace('\\', "/")
                .into_bytes()
        }
    } else {
        fs::read(&absolute)?
    };
    Ok(Some(
        EncodedObject::new(ObjectType::Blob, body).object_id(format)?,
    ))
}

fn apply_inexact_staged_renames(
    db: &FileObjectDatabase,
    output: &mut Vec<StatusOutputEntry>,
    rename_config: StatusRenameConfig,
) {
    fn read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Option<Vec<u8>> {
        db.read_object(oid).ok().map(|object| object.body.clone())
    }

    let is_staged_add = |output: &StatusOutputEntry| {
        output.rename_from.is_none()
            && output.entry.index == b'A'
            && output.entry.worktree == b' '
            && output.entry.index_oid.is_some()
            && !output.entry.index_mode.is_some_and(sley_index::is_gitlink)
    };
    let is_staged_delete = |output: &StatusOutputEntry| {
        output.rename_from.is_none()
            && output.entry.index == b'D'
            && output.entry.worktree == b' '
            && output.entry.head_oid.is_some()
            && !output.entry.head_mode.is_some_and(sley_index::is_gitlink)
    };
    let source_indices = output
        .iter()
        .enumerate()
        .filter(|(_, output)| is_staged_delete(output))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let target_indices = output
        .iter()
        .enumerate()
        .filter(|(_, output)| is_staged_add(output))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if source_indices.is_empty() || target_indices.is_empty() {
        return;
    }
    let source_blobs = source_indices
        .iter()
        .map(|&index| {
            output[index]
                .entry
                .head_oid
                .and_then(|oid| read_blob(db, &oid))
        })
        .collect::<Vec<_>>();
    let target_blobs = target_indices
        .iter()
        .map(|&index| {
            output[index]
                .entry
                .index_oid
                .and_then(|oid| read_blob(db, &oid))
        })
        .collect::<Vec<_>>();
    let mut pairs = Vec::new();
    for (target, target_blob) in target_blobs.iter().enumerate() {
        let Some(target_blob) = target_blob else {
            continue;
        };
        for (source, source_blob) in source_blobs.iter().enumerate() {
            let Some(source_blob) = source_blob else {
                continue;
            };
            pairs.push((
                sley_diff_merge::blob_similarity(source_blob, target_blob),
                target,
                source,
            ));
        }
    }
    pairs.sort_by_key(|pair| Reverse(pair.0));
    let mut target_paired = vec![None; target_indices.len()];
    if rename_config.detect == StatusRenameDetect::Copies {
        for &(score, target, source) in &pairs {
            if score < rename_config.copy_threshold {
                break;
            }
            if target_paired[target].is_none() {
                target_paired[target] = Some((source, true));
            }
        }
        let mut rename_target_for_source = vec![None; source_indices.len()];
        for (target, pairing) in target_paired.iter().enumerate() {
            if let Some((source, _)) = pairing {
                let slot = &mut rename_target_for_source[*source];
                if slot.is_none_or(|previous| target > previous) {
                    *slot = Some(target);
                }
            }
        }
        for target in rename_target_for_source.into_iter().flatten() {
            if let Some((source, _)) = target_paired[target] {
                target_paired[target] = Some((source, false));
            }
        }
    } else {
        let mut source_renamed = vec![false; source_indices.len()];
        for &(score, target, source) in &pairs {
            if score < rename_config.rename_threshold {
                break;
            }
            if target_paired[target].is_none() && !source_renamed[source] {
                target_paired[target] = Some((source, false));
                source_renamed[source] = true;
            }
        }
    }
    let mut remove = vec![false; output.len()];
    for (target, pairing) in target_paired.into_iter().enumerate() {
        let Some((source, is_copy)) = pairing else {
            continue;
        };
        let target_index = target_indices[target];
        let source_index = source_indices[source];
        let source = &output[source_index].entry;
        let source_path = source.path.clone();
        let source_head_mode = source.head_mode;
        let source_head_oid = source.head_oid;
        let target = &mut output[target_index];
        target.entry.index = if is_copy { b'C' } else { b'R' };
        target.entry.head_mode = source_head_mode;
        target.entry.head_oid = source_head_oid;
        target.rename_from = Some(source_path);
        if !is_copy {
            remove[source_index] = true;
        }
    }
    if remove.iter().any(|remove| *remove) {
        let mut index = 0;
        output.retain(|_| {
            let keep = !remove[index];
            index += 1;
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SubmoduleStatus;

    fn entry(index: u8, worktree: u8, path: &[u8]) -> ShortStatusEntry {
        ShortStatusEntry {
            index,
            worktree,
            path: path.to_vec(),
            head_mode: None,
            index_mode: None,
            worktree_mode: None,
            head_oid: None,
            index_oid: None,
            submodule: None,
        }
    }

    #[test]
    fn rename_config_precedence_is_typed() {
        let config = GitConfig::parse(b"[diff]\n\trenames = false\n[status]\n\trenames = copies\n")
            .expect("parse config");
        let resolved = resolve_status_rename_config(&config, None, Some(Some("75%".into())));
        assert_eq!(resolved.detect, StatusRenameDetect::Copies);
        assert_eq!(resolved.rename_threshold, 75);
        assert_eq!(resolved.copy_threshold, 75);
    }

    #[test]
    fn submodule_ignore_policy_clears_only_requested_dirt() {
        let config = GitConfig::default();
        let resolver = SubmoduleIgnoreResolver::load_for_worktree(
            None,
            &config,
            Some(IgnoreSubmodules::Dirty),
        );
        let mut row = entry(b' ', b'M', b"module");
        row.index_mode = Some(0o160000);
        row.submodule = Some(SubmoduleStatus {
            new_commits: true,
            modified_content: true,
            untracked_content: true,
        });
        assert!(apply_submodule_ignore_entry(&mut row, &resolver));
        assert_eq!(
            row.submodule,
            Some(SubmoduleStatus {
                new_commits: true,
                modified_content: false,
                untracked_content: false,
            })
        );
    }

    #[test]
    fn recursive_untracked_collapse_deduplicates_rows() {
        let mut entries = vec![entry(b'?', b'?', b"dir/a"), entry(b'?', b'?', b"dir/b")];
        let mut collapsed = BTreeMap::new();
        for mut row in entries.drain(..) {
            row.path = b"dir/".to_vec();
            collapsed
                .entry((row.index, row.worktree, row.path.clone()))
                .or_insert(row);
        }
        assert_eq!(collapsed.len(), 1);
    }

    #[test]
    fn planned_exact_rename_preserves_source_metadata() {
        let temp = tempfile::tempdir().expect("temporary worktree");
        let git_dir = temp.path().join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("object directory");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = EncodedObject::new(ObjectType::Blob, b"same\n".to_vec())
            .object_id(ObjectFormat::Sha1)
            .expect("blob oid");
        let mut deleted = entry(b'D', b' ', b"old");
        deleted.head_mode = Some(0o100644);
        deleted.head_oid = Some(oid);
        let mut added = entry(b'A', b' ', b"new");
        added.index_mode = Some(0o100644);
        added.index_oid = Some(oid);

        let planned = status_entries_with_renames(
            temp.path(),
            ObjectFormat::Sha1,
            &db,
            vec![deleted, added],
            StatusRenameConfig::default(),
        )
        .expect("plan rename");

        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].entry.index, b'R');
        assert_eq!(planned[0].entry.path, b"new");
        assert_eq!(planned[0].rename_from.as_deref(), Some(b"old".as_slice()));
        assert_eq!(planned[0].entry.head_oid, Some(oid));
    }
}
