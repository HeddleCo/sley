//! Repository layout helpers (object format, abbrev width, worktree root, pack counts).

use crate::{common_git_dir_for_git_dir, global_config_value, session};
use sley::plumbing::sley_odb::repository_objects_dir;
use sley::{GitConfig, GitError, ObjectFormat, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn worktree_prefix(
    cli_session: &session::CliSession,
    cwd: &Path,
    git_dir: &Path,
) -> Result<String> {
    let root = fs::canonicalize(worktree_root_for_git_dir(cli_session, git_dir)?)?;
    let cwd = fs::canonicalize(cwd)?;
    let prefix = cwd.strip_prefix(&root).map_err(|_| {
        GitError::InvalidPath(format!(
            "{} is outside worktree {}",
            cwd.display(),
            root.display()
        ))
    })?;
    if prefix.as_os_str().is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{}/", prefix.to_string_lossy().replace('\\', "/")))
}
pub(crate) fn repository_object_format(git_dir: &Path) -> Result<ObjectFormat> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let config = common_git_dir.join("config");
    let Ok(config) = GitConfig::read(config) else {
        return Ok(ObjectFormat::Sha1);
    };
    config.repository_object_format()
}
pub(crate) fn repository_abbrev(git_dir: &Path, format: ObjectFormat) -> Result<Option<usize>> {
    if let Some(value) = global_config_value("core.abbrev")? {
        return parse_repository_abbrev_value(git_dir, format, &value);
    }
    let config_path = git_dir.join("config");
    let Ok(config) = GitConfig::read(config_path) else {
        return Ok(Some(repository_auto_abbrev_width(git_dir, format)?));
    };
    let Some(value) = config.get("core", None, "abbrev") else {
        return Ok(Some(repository_auto_abbrev_width(git_dir, format)?));
    };
    parse_repository_abbrev_value(git_dir, format, value)
}

pub(crate) fn repository_abbrev_from_config(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<Option<usize>> {
    let Some(value) = config.get("core", None, "abbrev") else {
        return Ok(Some(repository_auto_abbrev_width(git_dir, format)?));
    };
    parse_repository_abbrev_value(git_dir, format, value)
}

fn parse_repository_abbrev_value(
    git_dir: &Path,
    format: ObjectFormat,
    value: &str,
) -> Result<Option<usize>> {
    if value.eq_ignore_ascii_case("no") {
        return Ok(None);
    }
    if value.eq_ignore_ascii_case("auto") {
        return Ok(Some(repository_auto_abbrev_width(git_dir, format)?));
    }
    let width = value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid core.abbrev value {value}")))?;
    if width < 4 {
        return Err(GitError::Command(format!(
            "core.abbrev length out of range: {width}"
        )));
    }
    Ok(Some(width.min(format.hex_len())))
}

fn repository_auto_abbrev_width(git_dir: &Path, format: ObjectFormat) -> Result<usize> {
    let object_count = repository_approx_object_count(git_dir, format)?;
    if object_count == 0 {
        return Ok(7.min(format.hex_len()));
    }
    let bits = u64::BITS as usize - object_count.saturating_sub(1).leading_zeros() as usize;
    Ok(bits.div_ceil(2).max(7).min(format.hex_len()))
}

fn repository_approx_object_count(git_dir: &Path, format: ObjectFormat) -> Result<u64> {
    let pack_dir = repository_objects_dir(git_dir).join("pack");
    if let Some(packed_count) =
        multi_pack_index_object_count(&pack_dir.join("multi-pack-index"), format)?
    {
        return Ok(packed_count);
    }
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(0);
    };
    let mut count = 0u64;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(std::ffi::OsStr::new("idx")) {
            continue;
        }
        count = count.saturating_add(u64::from(pack_index_object_count(&path, format)?));
    }
    Ok(count)
}

fn multi_pack_index_object_count(path: &Path, format: ObjectFormat) -> Result<Option<u64>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let midx = sley::plumbing::sley_pack::MultiPackIndex::parse_without_checksum(&bytes, format)?;
    Ok(Some(midx.objects.len() as u64))
}

fn pack_index_object_count(path: &Path, format_hash: ObjectFormat) -> Result<u32> {
    let bytes = fs::read(path)?;
    let index =
        sley::plumbing::sley_pack::PackIndexView::parse_trusted_without_checksum(&bytes, format_hash)?;
    Ok(index.count() as u32)
}
pub(crate) fn worktree_root_for_git_dir(
    cli_session: &session::CliSession,
    git_dir: &Path,
) -> Result<PathBuf> {
    cli_session.worktree_root_for_git_dir(git_dir)
}
