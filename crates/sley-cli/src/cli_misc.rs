//! Miscellaneous CLI helpers shared across porcelain commands.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sley::plumbing::sley_config::{ConfigEntry, ConfigSection};
use sley::plumbing::sley_object::{ObjectType, Tag};
use sley::plumbing::sley_odb::{FileObjectDatabase, ObjectReader};
use sley::plumbing::sley_refs::{FileRefStore, resolve_ref_peeled, validate_symref_name};
use sley::{GitConfig, GitError, Index, ObjectFormat, ObjectId, Result};

use crate::collect_short_status;
use crate::collect_short_status_with_options;
use crate::sley_index;
use crate::sley_worktree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AddAction {
    Add(PathBuf),
    Remove(PathBuf),
}

impl AddAction {
    pub(crate) fn path(&self) -> &PathBuf {
        match self {
            Self::Add(path) | Self::Remove(path) => path,
        }
    }
}

pub(crate) fn resolve_add_update_actions(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: Vec<PathBuf>,
    include_untracked: bool,
    ignore_missing: bool,
) -> Result<Vec<AddAction>> {
    let pathspecs = paths
        .into_iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(&path)
            };
            let matched = absolute.exists();
            (path, absolute, matched)
        })
        .collect::<Vec<_>>();
    let mut matched = pathspecs
        .iter()
        .map(|(_, _, matched)| *matched)
        .collect::<Vec<_>>();
    let status = if include_untracked {
        collect_short_status(worktree_root, git_dir, format)?
    } else {
        collect_short_status_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::ShortStatusOptions {
                untracked_mode: sley_worktree::StatusUntrackedMode::None,
                ..Default::default()
            },
        )?
    };
    let mut actions = Vec::new();
    for entry in status {
        if entry.index == b'?' && entry.worktree == b'?' {
            if !include_untracked {
                continue;
            }
        } else if entry.worktree != b'M'
            && entry.worktree != b'T'
            && entry.worktree != b'D'
            && entry.worktree != b'A'
        {
            // A typechange (`T`) stages like a modification: the path is re-added
            // with its new worktree mode/content (the `else` Add branch below).
            continue;
        }
        let path = worktree_root.join(
            std::str::from_utf8(&entry.path)
                .map_err(|err| GitError::InvalidPath(err.to_string()))?,
        );
        if !pathspecs.is_empty() {
            let mut path_matches = false;
            for (idx, (_, pathspec, _)) in pathspecs.iter().enumerate() {
                if add_path_matches(&path, pathspec) {
                    matched[idx] = true;
                    path_matches = true;
                }
            }
            if !path_matches {
                continue;
            }
        }
        if entry.worktree == b'D' {
            actions.push(AddAction::Remove(path));
        } else {
            actions.push(AddAction::Add(path));
        }
    }
    for ((display, _, _), matched) in pathspecs.iter().zip(matched) {
        if !matched && !ignore_missing {
            eprintln!(
                "fatal: pathspec '{}' did not match any files",
                display.to_string_lossy()
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(actions)
}

pub(crate) fn add_path_matches(path: &Path, pathspec: &Path) -> bool {
    let pathspec_text = pathspec.to_string_lossy();
    if sley_worktree::pathspec_is_glob(pathspec_text.as_bytes()) {
        let path_text = path.to_string_lossy();
        return sley_worktree::pathspec_item_matches(
            pathspec_text.as_bytes(),
            path_text.as_bytes(),
            sley_worktree::PathspecMatchMagic::default(),
        );
    }
    path == pathspec || path.starts_with(pathspec)
}

pub(crate) fn pack_refs_peeled_oid(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let mut current = *oid;
    let mut peeled = false;
    for _ in 0..16 {
        let object = db.read_object(&current)?;
        if object.object_type != ObjectType::Tag {
            return Ok(peeled.then_some(current));
        }
        let tag = Tag::parse_ref(format, &object.body)?;
        let target = db.read_object(&tag.object)?;
        if target.object_type != tag.object_type {
            return Ok(None);
        }
        current = tag.object;
        peeled = true;
    }
    Ok(None)
}

pub(crate) fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

pub(crate) fn count_objects_human_bytes(size_bytes: u64) -> String {
    if size_bytes == 0 {
        return "0 bytes".to_string();
    }
    if size_bytes < 1024 {
        return format!("{size_bytes} bytes");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut size = size_bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2} {}", UNITS[unit])
}

pub(crate) fn write_check_attr_state(
    stdout: &mut impl Write,
    state: Option<&sley_worktree::AttributeState>,
) -> Result<()> {
    match state {
        Some(sley_worktree::AttributeState::Set) => stdout.write_all(b"set")?,
        Some(sley_worktree::AttributeState::Unset) => stdout.write_all(b"unset")?,
        Some(sley_worktree::AttributeState::Value(value)) => stdout.write_all(value)?,
        None => stdout.write_all(b"unspecified")?,
    }
    Ok(())
}

pub(crate) fn check_ignore_tracked_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<(BTreeSet<Vec<u8>>, Vec<Vec<u8>>)> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok((BTreeSet::new(), Vec::new()));
    };
    let mut tracked = BTreeSet::new();
    let mut gitlinks = Vec::new();
    for entry in index.entries {
        let path = entry.path.into_bytes();
        if sley_index::is_gitlink(entry.mode) {
            gitlinks.push(path.clone());
        }
        tracked.insert(path);
    }
    Ok((tracked, gitlinks))
}

pub(crate) fn read_pathspecs_from_file(path: &Path, nul: bool) -> Result<Vec<PathBuf>> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        io::stdin().read_to_end(&mut bytes)?;
    } else {
        bytes = fs::read(path)?;
    }
    let separator = if nul { b'\0' } else { b'\n' };
    Ok(bytes
        .split(|byte| *byte == separator)
        .filter_map(|entry| {
            let entry = if !nul && entry.ends_with(b"\r") {
                &entry[..entry.len() - 1]
            } else {
                entry
            };
            if entry.is_empty() {
                return None;
            }
            // Git unquotes C-style quoted pathspecs read in LF mode (e.g.
            // `"file\101.t"` -> `fileA.t`); with `--pathspec-file-nul` the bytes
            // are taken verbatim, so a leading quote stays literal.
            if !nul && entry.first() == Some(&b'"') {
                let mut unquoted = Vec::new();
                if crate::commands::ref_command_stream::unquote_c_style(entry, &mut unquoted)
                    .is_some()
                {
                    return Some(PathBuf::from(
                        String::from_utf8_lossy(&unquoted).into_owned(),
                    ));
                }
            }
            Some(PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
        })
        .collect())
}

pub(crate) fn set_config_value(
    config: &mut GitConfig,
    name: &str,
    subsection: Option<&str>,
    key: &str,
    value: &str,
) {
    let section_idx = config
        .sections
        .iter()
        .rposition(|section| section.name == name && section.subsection.as_deref() == subsection)
        .unwrap_or_else(|| {
            config.sections.push(ConfigSection::new(
                name,
                subsection.map(str::to_string),
                Vec::new(),
            ));
            config.sections.len() - 1
        });
    let section = &mut config.sections[section_idx];
    if let Some(entry) = section
        .entries
        .iter_mut()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))
    {
        entry.value = Some(value.to_string());
        return;
    }
    section
        .entries
        .push(ConfigEntry::new(key, Some(value.to_string())));
}

pub(crate) fn submodule_worktree_has_untracked_entries(
    root: &Path,
    path: &Path,
    tracked: &BTreeSet<String>,
) -> Result<bool> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if submodule_worktree_has_untracked_entries(root, &entry_path, tracked)? {
                return Ok(true);
            }
            continue;
        }
        let relative = entry_path
            .strip_prefix(root)
            .map_err(|err| GitError::InvalidPath(err.to_string()))?
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if !tracked.contains(&relative) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn read_repository_index(git_dir: &Path, format: ObjectFormat) -> Result<Option<Index>> {
    sley_worktree::read_repository_index(git_dir, format)
}

pub(crate) fn resolve_ref_to_oid(store: &FileRefStore, name: &str) -> Result<Option<ObjectId>> {
    resolve_ref_peeled(store, name)
}

pub(crate) fn show_ref_filter_matches(name: &str, filter: &str) -> bool {
    name == filter
        || name
            .strip_suffix(filter)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

pub(crate) fn parse_abbrev(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid abbrev length {value}")))
}

pub(crate) fn delete_symbolic_ref(store: &FileRefStore, name: &str) -> Result<()> {
    if name == "HEAD" {
        return symbolic_ref_delete_head();
    }
    if validate_symref_name(name).is_err() {
        return symbolic_ref_cannot_delete(name);
    }
    if store.delete_symbolic_ref(name)? {
        return Ok(());
    }
    symbolic_ref_cannot_delete(name)
}

pub(crate) fn symbolic_ref_delete_head() -> Result<()> {
    eprintln!("fatal: deleting 'HEAD' is not allowed");
    Err(GitError::Exit(128))
}

pub(crate) fn symbolic_ref_cannot_delete(name: &str) -> Result<()> {
    eprintln!("fatal: Cannot delete {name}, not a symbolic ref");
    Err(GitError::Exit(128))
}
