//! Repository layout helpers (object format, abbrev width, worktree root, pack counts).

use crate::{
    common_git_dir_for_git_dir, explicit_git_dir, explicit_work_tree, global_config_value,
    resolve_cli_path, sley_worktree,
};
use sley::plumbing::sley_odb::repository_objects_dir;
use sley::{GitConfig, GitError, ObjectFormat, Result};
use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub(crate) fn worktree_prefix(cwd: &Path, git_dir: &Path) -> Result<String> {
    let root = fs::canonicalize(worktree_root_for_git_dir(git_dir)?)?;
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
    if let Some(value) = global_config_value("core.abbrev")? {
        return parse_repository_abbrev_value(git_dir, format, &value);
    }
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
    Ok(((bits + 1) / 2).max(7).min(format.hex_len()))
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
        count = count.saturating_add(u64::from(pack_index_object_count(&path)?));
    }
    Ok(count)
}

fn multi_pack_index_object_count(path: &Path, format: ObjectFormat) -> Result<Option<u64>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut header = [0u8; 12];
    file.read_exact(&mut header).map_err(|_| {
        GitError::InvalidFormat(format!("multi-pack-index {} is too short", path.display()))
    })?;
    if &header[..4] != b"MIDX" {
        return Err(GitError::InvalidFormat(format!(
            "missing multi-pack-index signature in {}",
            path.display()
        )));
    }
    let version = header[4];
    if version != 1 && version != 2 {
        return Err(GitError::Unsupported(format!(
            "multi-pack-index version {version}"
        )));
    }
    let expected_hash_id = match format {
        ObjectFormat::Sha1 => 1,
        ObjectFormat::Sha256 => 2,
    };
    let hash_id = header[5];
    if u32::from(hash_id) != expected_hash_id {
        return Err(GitError::InvalidFormat(format!(
            "multi-pack-index hash id {hash_id} does not match {}",
            format.name()
        )));
    }
    let chunk_count = header[6] as usize;
    let base_midx_count = header[7];
    if base_midx_count != 0 {
        return Err(GitError::Unsupported(format!(
            "multi-pack-index base count {base_midx_count}"
        )));
    }

    let mut lookup = vec![0u8; (chunk_count + 1).saturating_mul(12)];
    file.read_exact(&mut lookup).map_err(|_| {
        GitError::InvalidFormat(format!(
            "truncated multi-pack-index chunk lookup in {}",
            path.display()
        ))
    })?;
    let mut oid_fanout_offset = None;
    for chunk in lookup.chunks_exact(12).take(chunk_count) {
        if &chunk[..4] == b"OIDF" {
            oid_fanout_offset = Some(u64::from_be_bytes([
                chunk[4], chunk[5], chunk[6], chunk[7], chunk[8], chunk[9], chunk[10], chunk[11],
            ]));
            break;
        }
    }
    let Some(oid_fanout_offset) = oid_fanout_offset else {
        return Err(GitError::InvalidFormat(format!(
            "multi-pack-index {} missing OIDF chunk",
            path.display()
        )));
    };
    file.seek(SeekFrom::Start(oid_fanout_offset + 255 * 4))?;
    let mut count = [0u8; 4];
    file.read_exact(&mut count).map_err(|_| {
        GitError::InvalidFormat(format!(
            "truncated multi-pack-index OIDF chunk in {}",
            path.display()
        ))
    })?;
    Ok(Some(u64::from(u32::from_be_bytes(count))))
}

fn pack_index_object_count(path: &Path) -> Result<u32> {
    let mut file = fs::File::open(path)?;
    let mut header = [0u8; 8 + 256 * 4];
    file.read_exact(&mut header[..8]).map_err(|_| {
        GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
    })?;
    let fanout_offset = if header[..8].starts_with(&[0xff, b't', b'O', b'c']) {
        file.read_exact(&mut header[8..]).map_err(|_| {
            GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
        })?;
        8
    } else {
        file.read_exact(&mut header[8..256 * 4]).map_err(|_| {
            GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
        })?;
        0
    };
    let offset = fanout_offset + 255 * 4;
    Ok(u32::from_be_bytes([
        header[offset],
        header[offset + 1],
        header[offset + 2],
        header[offset + 3],
    ]))
}
pub(crate) fn worktree_root_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    // CLI/process-level overrides take precedence over anything recorded in the
    // repository (these are not part of the repository-intrinsic resolution).
    if let Some(work_tree) = explicit_work_tree() {
        let work_tree =
            resolve_cli_path(&env::current_dir()?, work_tree.to_string_lossy().as_ref());
        return fs::canonicalize(work_tree).map_err(|err| GitError::Io(err.to_string()));
    }
    // Repository-intrinsic layout handles core.worktree, linked worktrees, and
    // the normal parent-of-.git case without consulting invocation globals.
    if let Some(root) = sley_worktree::worktree_root_for_git_dir(git_dir)? {
        return Ok(root);
    }
    if explicit_git_dir().is_some() {
        return env::current_dir().map_err(|err| GitError::Io(err.to_string()));
    }
    Err(GitError::Unsupported(
        "update-index currently requires a non-bare worktree".into(),
    ))
}
