//! Prune walks: root collection (refs, HEAD, linked-worktree indexes,
//! reflogs, rebase state), recent-object grace handling, expired loose
//! pruning, temporary-file and empty-directory cleanup, shallow repair, and
//! the `gc.recentObjectsHook` runner.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_odb::{repository_objects_dir, FileObjectDatabase};
use sley_refs::FileRefStore;

use crate::{parse_reflog_expire_time, read_repo_config, resolve_ref_to_oid, resolve_revision};

pub fn parse_prune_expire(value: &str, option: &str) -> Result<i64> {
    match value {
        "now" | "all" => Ok(i64::MAX),
        "never" => Ok(i64::MIN),
        _ => parse_reflog_expire_time(value, option).map_err(|err| {
            if matches!(err, GitError::Exit(_)) {
                eprintln!("error: malformed expiration date '{value}'");
            }
            err
        }),
    }
}

pub fn prune_roots(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    heads: &[String],
) -> Result<Vec<ObjectId>> {
    let store = FileRefStore::new(common_git_dir, format);
    let mut roots = BTreeSet::new();
    for reference in store.list_refs()? {
        if let Some(oid) = resolve_ref_to_oid(&store, &reference.name)? {
            roots.insert(oid);
        }
    }
    for worktree_git_dir in prune_worktree_git_dirs(git_dir, common_git_dir)? {
        if let Some(oid) = prune_head_root(&store, &worktree_git_dir, format)? {
            roots.insert(oid);
        }
        for oid in prune_index_roots(&worktree_git_dir, format)? {
            roots.insert(oid);
        }
        for oid in reflog_roots_from_dir(&worktree_git_dir.join("logs"), format)? {
            roots.insert(oid);
        }
        for oid in prune_state_file_roots(&worktree_git_dir, format)? {
            roots.insert(oid);
        }
    }
    for head in heads {
        roots.insert(resolve_revision(
            common_git_dir,
            format,
            head,
            replace_objects,
        )?);
    }
    roots.extend(reflog_roots_from_dir(&common_git_dir.join("logs"), format)?);
    Ok(roots.into_iter().collect())
}

pub fn prune_worktree_git_dirs(git_dir: &Path, common_git_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![git_dir.to_path_buf()];
    if git_dir != common_git_dir {
        dirs.push(common_git_dir.to_path_buf());
    }
    let worktrees = common_git_dir.join("worktrees");
    if let Ok(entries) = fs::read_dir(worktrees) {
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

pub fn prune_head_root(
    store: &FileRefStore,
    worktree_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<ObjectId>> {
    let Ok(head) = fs::read_to_string(worktree_git_dir.join("HEAD")) else {
        return Ok(None);
    };
    let head = head.trim();
    if let Some(refname) = head.strip_prefix("ref:") {
        return resolve_ref_to_oid(store, refname.trim());
    }
    if head.len() == format.hex_len() {
        return ObjectId::from_hex(format, head).map(Some);
    }
    Ok(None)
}

pub fn prune_index_roots(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let bytes = match fs::read(git_dir.join("index")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let index = sley_index::Index::parse(&bytes, format)?;
    Ok(index
        .entries
        .into_iter()
        .filter(|entry| !sley_index::is_gitlink(entry.mode))
        .map(|entry| entry.oid)
        .collect())
}

pub fn prune_state_file_roots(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    for path in [
        "rebase-apply/autostash",
        "rebase-apply/orig-head",
        "rebase-merge/autostash",
        "rebase-merge/orig-head",
    ] {
        let Ok(contents) = fs::read_to_string(git_dir.join(path)) else {
            continue;
        };
        let value = contents.trim();
        if value.len() == format.hex_len()
            && let Ok(oid) = ObjectId::from_hex(format, value)
        {
            roots.push(oid);
        }
    }
    Ok(roots)
}

pub fn reflog_roots_from_dir(logs_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    let zero = vec![b'0'; format.hex_len()];
    let mut stack: Vec<PathBuf> = vec![logs_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound && dir == logs_dir => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                let contents = fs::read(&path)?;
                for line in contents.split(|byte| *byte == b'\n') {
                    let mut fields = line.split(|byte| *byte == b' ');
                    for hex in [fields.next(), fields.next()].into_iter().flatten() {
                        if hex != zero
                            && let Ok(hex) = std::str::from_utf8(hex)
                            && let Ok(oid) = ObjectId::from_hex(format, hex)
                        {
                            roots.push(oid);
                        }
                    }
                }
            }
        }
    }
    Ok(roots)
}

pub fn prune_recent_object_roots(
    db: &FileObjectDatabase,
    common_git_dir: &Path,
    format: ObjectFormat,
    expire: i64,
) -> Result<Vec<ObjectId>> {
    if expire == i64::MIN {
        return Ok(Vec::new());
    }
    let mut roots = Vec::new();
    let object_mtimes = sley_odb::object_mtimes_on_disk_pub(
        &sley_odb::repository_objects_dir(common_git_dir),
        format,
    )?;
    for (oid, mtime) in object_mtimes {
        if i64::from(mtime) <= expire {
            continue;
        }
        // Presence probe only: these ids come from the loose-object walk, so
        // inflating full objects just to test existence is wasted work.
        if db.loose().exists(&oid)? {
            roots.push(oid);
        }
    }
    Ok(roots)
}

pub fn prune_recent_hook_roots(
    common_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let config = read_repo_config(common_git_dir)?;
    run_recent_objects_hooks(
        &config,
        format,
        common_git_dir.parent().unwrap_or(common_git_dir),
    )
}

/// Run every configured `gc.recentObjectsHook`, collecting the object ids each
/// invocation prints on stdout. Hook stderr passes through byte-for-byte.
pub fn run_recent_objects_hooks(
    config: &GitConfig,
    format: ObjectFormat,
    cwd: &Path,
) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    for hook in config
        .get_all("gc", None, "recentObjectsHook")
        .into_iter()
        .flatten()
    {
        let mut command = recent_objects_hook_command(hook);
        let output = command
            .current_dir(cwd)
            // Hook stdout is the object-id protocol; stderr remains user-facing
            // and must pass through byte-for-byte, including on failure.
            .stderr(std::process::Stdio::inherit())
            .output()?;
        if !output.status.success() {
            eprintln!("fatal: unable to enumerate additional recent objects");
            return Err(GitError::Exit(128));
        }
        for line in output.stdout.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            let value = std::str::from_utf8(line).map_err(|_| {
                GitError::InvalidFormat("invalid object ID from gc.recentObjectsHook".into())
            })?;
            roots.push(ObjectId::from_hex(format, value)?);
        }
    }
    Ok(roots)
}

fn recent_objects_hook_command(script: &str) -> Command {
    if let Some(shell) = env::var_os("GIT_SHELL_PATH") {
        let mut command = Command::new(shell);
        command.arg("-c").arg(script);
        return command;
    }
    #[cfg(windows)]
    {
        let shell = env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.arg("/C").arg(script);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }
}


pub fn prune_object_is_expired(db: &FileObjectDatabase, oid: &ObjectId, expire: i64) -> Result<bool> {
    if expire == i64::MIN {
        return Ok(false);
    }
    if expire == i64::MAX {
        return Ok(true);
    }
    let path = db.loose().object_path(oid)?;
    let Some(mtime) = fs::metadata(path).ok().as_ref().and_then(file_mtime_seconds) else {
        // An object we cannot stat must fail closed toward preservation, the
        // way upstream skips objects whose mtime it cannot read.
        return Ok(false);
    };
    Ok(mtime <= expire)
}

pub fn prune_temporary_files(path: &Path, expire: i64, dry_run: bool, verbose: bool) -> Result<()> {
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("tmp_") {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        // An unreadable mtime fails closed toward preservation: skip the entry
        // rather than treating it as ancient.
        if file_mtime_seconds(&metadata).is_none_or(|mtime| mtime > expire) {
            continue;
        }
        if dry_run || verbose {
            if metadata.is_dir() {
                println!("Removing stale temporary directory {}", path.display());
            } else {
                println!("Removing stale temporary file {}", path.display());
            }
        }
        if dry_run {
            continue;
        }
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Seconds since the unix epoch for the metadata's mtime, or `None` when the
/// timestamp cannot be read (callers must fail closed toward preservation).
fn file_mtime_seconds(metadata: &fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
}

pub fn prune_packed_loose_objects(git_dir: &Path, format: ObjectFormat, dry_run: bool) -> Result<()> {
    let objects_dir = repository_objects_dir(git_dir);
    let packed = sley_odb::packed_object_ids(&objects_dir, format)?;
    if packed.is_empty() {
        return Ok(());
    }
    for (oid, path) in prune_loose_object_paths(&objects_dir, format)? {
        if !packed.contains(&oid) || dry_run {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    if !dry_run {
        prune_empty_loose_object_dirs(&objects_dir)?;
    }
    Ok(())
}

pub fn prune_loose_object_paths(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<(ObjectId, PathBuf)>> {
    let mut objects = Vec::new();
    if !objects_dir.exists() {
        return Ok(objects);
    }
    let hex_len = format.hex_len();
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let fanout = entry.file_name();
        let Some(fanout) = fanout.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for object_entry in fs::read_dir(entry.path())? {
            let object_entry = object_entry?;
            if !object_entry.file_type()?.is_file() {
                continue;
            }
            let suffix = object_entry.file_name();
            let Some(suffix) = suffix.to_str() else {
                continue;
            };
            if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let oid = ObjectId::from_hex(format, &format!("{fanout}{suffix}"))?;
            objects.push((oid, object_entry.path()));
        }
    }
    objects.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(objects)
}

pub fn prune_empty_loose_object_dirs(objects_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let _ = fs::remove_dir(entry.path());
        }
    }
    Ok(())
}

pub(crate) fn prune_repack_shallow_file(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
) -> Result<()> {
    // Filter repacks intentionally leave large blobs absent from the local ODB
    // (`--filter-to`). Only a present `shallow` file needs a reachability walk,
    // and even then missing objects must be tolerated so filtered-out blobs do
    // not abort an otherwise successful repack.
    if !git_dir.join("shallow").exists() {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let reachable = sley_odb::collect_reachable_object_ids_tolerating_missing(
        &db,
        format,
        roots.iter().copied(),
    )?;
    prune_shallow_file(git_dir, format, &reachable, false, false)
}

pub fn prune_shallow_file(
    git_dir: &Path,
    format: ObjectFormat,
    reachable: &HashSet<ObjectId>,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    let path = git_dir.join("shallow");
    let Ok(contents) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let mut retained = Vec::new();
    let mut removed = Vec::new();
    for line in contents.lines() {
        let value = line.trim();
        if value.len() != format.hex_len() {
            retained.push(value.to_string());
            continue;
        }
        let oid = ObjectId::from_hex(format, value)?;
        if reachable.contains(&oid) {
            retained.push(value.to_string());
        } else {
            removed.push(oid);
        }
    }
    if (dry_run || verbose) && !removed.is_empty() {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        for oid in &removed {
            let type_name = db
                .read_object_header(oid)?
                .map(|(object_type, _size)| object_type.as_str())
                .unwrap_or("unknown");
            println!("{oid} {type_name}");
        }
    }
    if dry_run || removed.is_empty() {
        return Ok(());
    }
    if retained.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    } else {
        let mut out = retained.join("\n");
        out.push('\n');
        fs::write(path, out)?;
    }
    Ok(())
}
