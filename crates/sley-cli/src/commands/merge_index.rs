//! `git merge-index <merge-program> (-a | [--] <path>...)` — run a per-file
//! merge program over every unmerged index entry.
//!
//! For each unmerged path the entry's stages (1=base, 2=ours, 3=theirs) are
//! collected and the program is invoked as
//! `<program> <sha1> <sha2> <sha3> <path> <mode1> <mode2> <mode3>` (empty fields
//! for absent stages), mirroring `builtin/merge-index.c`. The `git-merge-one-file`
//! program — git's per-file 3-way merge driver — is provided here as a builtin so
//! it works without an external `git-core` exec dir; any other program is run as
//! a real child process.

use crate::commands::merge_rebase::{
    merge_index_entry, merge_read_blob, merge_write_worktree_file, read_worktree_index,
};
use crate::*;

/// The repository-relative path bytes of an index entry.
fn entry_path(entry: &sley_index::IndexEntry) -> &[u8] {
    entry.path.as_ref()
}

/// The merge stage (0..=3) encoded in an index entry's flags.
fn entry_stage(entry: &sley_index::IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

/// The stages (mode, oid) of one conflicted path.
#[derive(Default)]
struct MergeIndexStages {
    base: Option<(u32, ObjectId)>,
    ours: Option<(u32, ObjectId)>,
    theirs: Option<(u32, ObjectId)>,
}

pub(crate) fn cmd_merge_index(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut idx = 0;
    let mut one_shot = false;
    let mut quiet = false;
    if args.get(idx).map(String::as_str) == Some("-o") {
        one_shot = true;
        idx += 1;
    }
    if args.get(idx).map(String::as_str) == Some("-q") {
        quiet = true;
        idx += 1;
    }
    let Some(program) = args.get(idx).cloned() else {
        eprintln!("usage: git merge-index [-o] [-q] <merge-program> (-a | [--] [<filename>...])");
        return Err(GitError::Exit(129));
    };
    idx += 1;

    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let mut index = read_worktree_index(&git_dir, format)?;

    // Collect the work list up front; the merge program rewrites index entries,
    // so we must not iterate the live entry vector.
    let mut paths: Vec<Vec<u8>> = Vec::new();
    let mut force_file = false;
    for arg in &args[idx..] {
        if !force_file && arg.starts_with('-') {
            match arg.as_str() {
                "--" => force_file = true,
                "-a" => {
                    for path in merge_index_unmerged_paths(&index) {
                        if !paths.contains(&path) {
                            paths.push(path);
                        }
                    }
                }
                other => {
                    eprintln!("git merge-index: unknown option {other}");
                    return Err(GitError::Exit(128));
                }
            }
            continue;
        }
        let path = arg.clone().into_bytes();
        // git only runs the program when the path is not already merged (stage 0
        // absent); a fully merged path is silently skipped.
        if merge_index_stage_present(&index, &path, 0) {
            continue;
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }

    let mut errors = 0u32;
    for path in &paths {
        if !run_merge_program(&program, &db, &git_dir, format, &mut index, path, quiet)? {
            errors += 1;
            if !one_shot {
                if !quiet {
                    eprintln!("fatal: merge program failed");
                }
                write_merge_index(&git_dir, format, &index)?;
                return Err(GitError::Exit(1));
            }
        }
    }

    write_merge_index(&git_dir, format, &index)?;
    if errors > 0 {
        if !quiet {
            eprintln!("fatal: merge program failed");
        }
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn merge_index_unmerged_paths(index: &sley_index::Index) -> Vec<Vec<u8>> {
    let mut paths = Vec::new();
    for entry in &index.entries {
        if entry_stage(entry) != 0 {
            let path = entry_path(entry).to_vec();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn merge_index_stage_present(index: &sley_index::Index, path: &[u8], stage: u16) -> bool {
    index
        .entries
        .iter()
        .any(|entry| entry_path(entry) == path && entry_stage(entry) == stage)
}

fn collect_stages(index: &sley_index::Index, path: &[u8]) -> MergeIndexStages {
    let mut stages = MergeIndexStages::default();
    for entry in &index.entries {
        if entry_path(entry) != path {
            continue;
        }
        let slot = match entry_stage(entry) {
            1 => &mut stages.base,
            2 => &mut stages.ours,
            3 => &mut stages.theirs,
            _ => continue,
        };
        *slot = Some((entry.mode, entry.oid));
    }
    stages
}

/// Run the configured merge program for one path. Returns `Ok(true)` on success,
/// `Ok(false)` when the program reported a merge failure.
fn run_merge_program(
    program: &str,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    index: &mut sley_index::Index,
    path: &[u8],
    quiet: bool,
) -> Result<bool> {
    let stages = collect_stages(index, path);
    let basename = program.rsplit(['/', '\\']).next().unwrap_or(program);
    if matches!(basename, "git-merge-one-file" | "merge-one-file") {
        merge_one_file(db, git_dir, format, index, path, &stages)
    } else {
        run_external_merge_program(program, path, &stages, quiet)
    }
}

/// git's `git-merge-one-file` driver, reduced to the cases its shell script
/// handles. Returns `Ok(false)` on a conflict / unmergeable case (the caller
/// turns that into the same non-zero exit git's `run_command` failure would).
fn merge_one_file(
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    index: &mut sley_index::Index,
    path: &[u8],
    stages: &MergeIndexStages,
) -> Result<bool> {
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let path_str = String::from_utf8_lossy(path).into_owned();

    match (stages.base, stages.ours, stages.theirs) {
        // Added in our branch only: nothing for the other side to merge.
        (None, Some((mode, oid)), None) => {
            set_stage0(index, path, mode, oid);
            Ok(true)
        }
        // Added in their branch only: stage and materialise it.
        (None, None, Some((mode, oid))) => {
            println!("Adding {path_str}");
            let content = merge_read_blob(db, &oid)?;
            merge_write_worktree_file(&worktree_root, path, &content, mode)?;
            set_stage0(index, path, mode, oid);
            Ok(true)
        }
        // Added identically in both branches.
        (None, Some((our_mode, our_oid)), Some((their_mode, their_oid)))
            if our_oid == their_oid =>
        {
            if our_mode != their_mode {
                eprintln!("ERROR: File {path_str} added identically in both branches,");
                eprintln!("ERROR: but permissions conflict {our_mode:o}->{their_mode:o}.");
                return Ok(false);
            }
            println!("Adding {path_str}");
            let content = merge_read_blob(db, &our_oid)?;
            merge_write_worktree_file(&worktree_root, path, &content, our_mode)?;
            set_stage0(index, path, our_mode, our_oid);
            Ok(true)
        }
        // Deleted in both, or deleted on one side and unchanged on the other.
        (Some((_, base_oid)), ours, theirs)
            if ours.is_none_or(|(_, oid)| oid == base_oid)
                && theirs.is_none_or(|(_, oid)| oid == base_oid)
                && (ours.is_none() || theirs.is_none()) =>
        {
            remove_path(index, &worktree_root, path)?;
            Ok(true)
        }
        // Modified on both sides (base present or both added differently).
        (base, Some((our_mode, our_oid)), Some((their_mode, their_oid))) => {
            if our_mode == 0o120000 || their_mode == 0o120000 {
                eprintln!("ERROR: {path_str}: Not merging symbolic link changes.");
                return Ok(false);
            }
            if our_mode == 0o160000 || their_mode == 0o160000 {
                eprintln!("ERROR: {path_str}: Not merging conflicting submodule changes.");
                return Ok(false);
            }
            let base_content = match base {
                Some((_, oid)) => merge_read_blob(db, &oid)?,
                None => Vec::new(),
            };
            let our_content = merge_read_blob(db, &our_oid)?;
            let their_content = merge_read_blob(db, &their_oid)?;
            if base.is_some() {
                println!("Auto-merging {path_str}");
            } else {
                println!("Added {path_str} in both, but differently.");
            }
            let merged = sley_diff_merge::merge_blobs(
                &base_content,
                &our_content,
                &their_content,
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
            // The working tree always gets the merge result (markers and all),
            // matching git-merge-one-file's `cat "$src1" >"$4"`.
            merge_write_worktree_file(&worktree_root, path, &merged.content, our_mode)?;
            let mut conflict = merged.conflicted || base.is_none();
            let mut message = if conflict { "content conflict" } else { "" }.to_string();
            if our_mode != their_mode {
                if !message.is_empty() {
                    message.push_str(", ");
                }
                message.push_str(&format!(
                    "permissions conflict: {:o}->{our_mode:o},{their_mode:o}",
                    base.map(|(mode, _)| mode).unwrap_or(0)
                ));
                conflict = true;
            }
            if conflict {
                eprintln!("ERROR: {message} in {path_str}");
                return Ok(false);
            }
            let oid = db.write_object(EncodedObject::new(ObjectType::Blob, merged.content))?;
            set_stage0(index, path, our_mode, oid);
            Ok(true)
        }
        _ => {
            eprintln!(
                "ERROR: {path_str}: Not handling case {} -> {} -> {}",
                stages.base.map(|(_, o)| o.to_hex()).unwrap_or_default(),
                stages.ours.map(|(_, o)| o.to_hex()).unwrap_or_default(),
                stages.theirs.map(|(_, o)| o.to_hex()).unwrap_or_default(),
            );
            Ok(false)
        }
    }
}

/// Replace every stage entry for `path` with a single stage-0 entry.
fn set_stage0(index: &mut sley_index::Index, path: &[u8], mode: u32, oid: ObjectId) {
    index.entries.retain(|entry| entry_path(entry) != path);
    index.entries.push(merge_index_entry(path, mode, oid, 0));
    sort_index_entries(index);
}

fn remove_path(index: &mut sley_index::Index, worktree_root: &Path, path: &[u8]) -> Result<()> {
    index.entries.retain(|entry| entry_path(entry) != path);
    let rel = String::from_utf8_lossy(path);
    let full = worktree_root.join(rel.as_ref());
    match fs::remove_file(&full) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

fn sort_index_entries(index: &mut sley_index::Index) {
    index.entries.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| entry_stage(a).cmp(&entry_stage(b)))
    });
}

fn write_merge_index(
    git_dir: &Path,
    format: ObjectFormat,
    index: &sley_index::Index,
) -> Result<()> {
    let bytes = index.write(format)?;
    let index_path = sley_worktree::repository_index_path(git_dir);
    fs::write(index_path, bytes)?;
    Ok(())
}

fn run_external_merge_program(
    program: &str,
    path: &[u8],
    stages: &MergeIndexStages,
    quiet: bool,
) -> Result<bool> {
    let hex =
        |stage: Option<(u32, ObjectId)>| stage.map(|(_, oid)| oid.to_hex()).unwrap_or_default();
    let mode = |stage: Option<(u32, ObjectId)>| {
        stage
            .map(|(mode, _)| format!("{mode:o}"))
            .unwrap_or_default()
    };
    #[cfg(unix)]
    let path_arg: std::ffi::OsString = {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(path).to_os_string()
    };
    #[cfg(not(unix))]
    let path_arg: std::ffi::OsString = String::from_utf8_lossy(path).into_owned().into();
    let mut command = std::process::Command::new(program);
    command
        .arg(hex(stages.base))
        .arg(hex(stages.ours))
        .arg(hex(stages.theirs))
        .arg(&path_arg)
        .arg(mode(stages.base))
        .arg(mode(stages.ours))
        .arg(mode(stages.theirs));
    match command.status() {
        Ok(status) => Ok(status.success()),
        Err(err) => {
            if !quiet {
                eprintln!("error: cannot run {program}: {err}");
            }
            Ok(false)
        }
    }
}
