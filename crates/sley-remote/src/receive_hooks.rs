//! Traditional receive-pack hooks (`pre-receive`, `update`, `post-receive`, `post-update`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sley_core::{GitError, Result};
use sley_odb::repository_common_dir;
use sley_protocol::ReceivePackCommand;

use crate::proc_receive::ReceivePackCommandState;

pub fn run_pre_receive(
    git_dir: &Path,
    commands: &[ReceivePackCommand],
    push_options: &[String],
    quarantine_env: &[(String, String)],
    remote_stderr: &mut Vec<u8>,
    capture_stderr: bool,
) -> Result<()> {
    let Some(path) = find_hook_path(git_dir, "pre-receive") else {
        return Ok(());
    };
    let stdin = receive_hook_stdin(commands);
    spawn_hook(
        git_dir,
        &path,
        &[],
        Some(&stdin),
        &receive_hook_env(push_options, quarantine_env),
        remote_stderr,
        capture_stderr,
    )
}

pub fn run_update_hooks(
    git_dir: &Path,
    commands: &[ReceivePackCommand],
    quarantine_env: &[(String, String)],
    remote_stderr: &mut Vec<u8>,
    capture_stderr: bool,
) -> Result<Option<String>> {
    let env = receive_hook_env(&[], quarantine_env);
    for command in receive_update_hook_order(commands) {
        let Some(path) = find_hook_path(git_dir, "update") else {
            continue;
        };
        let args = [
            command.name.as_str(),
            &command.old_id.to_string(),
            &command.new_id.to_string(),
        ];
        if let Err(err) = spawn_hook(
            git_dir,
            &path,
            &args,
            None,
            &env,
            remote_stderr,
            capture_stderr,
        ) {
            if matches!(err, GitError::Exit(_)) {
                return Ok(Some(command.name.clone()));
            }
            return Err(err);
        }
    }
    Ok(None)
}

pub fn run_post_receive(
    git_dir: &Path,
    commands: &[ReceivePackCommandState],
    push_options: &[String],
    remote_stderr: &mut Vec<u8>,
    capture_stderr: bool,
) -> Result<()> {
    let Some(path) = find_hook_path(git_dir, "post-receive") else {
        return Ok(());
    };
    let stdin = post_receive_hook_stdin(commands);
    spawn_hook(
        git_dir,
        &path,
        &[],
        Some(&stdin),
        &receive_hook_env(push_options, &[]),
        remote_stderr,
        capture_stderr,
    )
}

pub fn run_post_update(
    git_dir: &Path,
    commands: &[ReceivePackCommandState],
    remote_stderr: &mut Vec<u8>,
    capture_stderr: bool,
) -> Result<()> {
    let Some(path) = find_hook_path(git_dir, "post-update") else {
        return Ok(());
    };
    let args: Vec<String> = receive_stream_hook_order(commands)
        .iter()
        .map(|state| state.command.name.clone())
        .collect();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    spawn_hook(
        git_dir,
        &path,
        &arg_refs,
        None,
        &[],
        remote_stderr,
        capture_stderr,
    )
}

/// Run the `push-to-checkout` hook with the new tip oid as argv[1].
///
/// Returns `Ok(true)` when the hook ran, `Ok(false)` when no hook is installed.
/// A non-zero hook exit becomes a `GitError::Command` with the git wording
/// (`push-to-checkout hook declined`).
pub fn run_push_to_checkout_hook(
    git_dir: &Path,
    new_oid: &sley_core::ObjectId,
    worktree: &Path,
    remote_stderr: &mut Vec<u8>,
    capture_stderr: bool,
) -> Result<bool> {
    let Some(path) = find_hook_path(git_dir, "push-to-checkout") else {
        return Ok(false);
    };
    let oid = new_oid.to_string();
    let env = vec![
        (
            "GIT_WORK_TREE".into(),
            worktree.to_string_lossy().into_owned(),
        ),
        ("GIT_DIR".into(), git_dir.to_string_lossy().into_owned()),
    ];
    match spawn_hook(
        git_dir,
        &path,
        &[&oid],
        None,
        &env,
        remote_stderr,
        capture_stderr,
    ) {
        Ok(()) => Ok(true),
        Err(GitError::Command(_)) | Err(GitError::Exit(_)) => Err(GitError::Command(
            "push-to-checkout hook declined".into(),
        )),
        Err(err) => Err(err),
    }
}

/// Legacy no-arg form used by post-update side paths (t1800 stdout capture).
pub fn run_push_to_checkout(
    git_dir: &Path,
    remote_stderr: &mut Vec<u8>,
    capture_stderr: bool,
) -> Result<()> {
    let Some(path) = find_hook_path(git_dir, "push-to-checkout") else {
        return Ok(());
    };
    spawn_hook(
        git_dir,
        &path,
        &[],
        None,
        &[],
        remote_stderr,
        capture_stderr,
    )
}

/// `receive.denyCurrentBranch=updateInstead` worktree update (git's
/// `update_worktree` / `push_to_deploy`).
///
/// Prefer the `push-to-checkout` hook when present; otherwise refresh the index,
/// refuse dirty worktrees / staged changes, refuse untracked paths that the new
/// tree would overwrite, then hard-reset the index+worktree to `new_oid`.
pub fn update_worktree_for_update_instead(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    new_oid: &sley_core::ObjectId,
    remote_stderr: &mut Vec<u8>,
) -> Result<()> {
    let Some(worktree) = sley_worktree::worktree_root_for_git_dir(git_dir)? else {
        return Err(GitError::Command(
            "denyCurrentBranch = updateInstead needs a worktree".into(),
        ));
    };
    // Hook takes precedence (receive-pack.c push_to_checkout).
    if run_push_to_checkout_hook(git_dir, new_oid, &worktree, remote_stderr, true)? {
        return Ok(());
    }
    push_to_deploy(git_dir, &worktree, format, new_oid)
}

/// Default updateInstead path when no push-to-checkout hook is installed.
fn push_to_deploy(
    git_dir: &Path,
    worktree: &Path,
    format: sley_core::ObjectFormat,
    new_oid: &sley_core::ObjectId,
) -> Result<()> {
    // update-index --refresh equivalent (git push_to_deploy step 1).
    let _ = sley_worktree::refresh_index_paths(
        worktree,
        git_dir,
        format,
        &[],
        true,  // quiet
        true,  // ignore_missing
        false, // really_refresh
    );

    // diff-files --quiet: refuse unstaged worktree changes.
    let modified = sley_worktree::modified_index_entries(worktree, git_dir, format)?;
    let deleted = sley_worktree::deleted_index_entries(worktree, git_dir, format)?;
    if !modified.is_empty() || !deleted.is_empty() {
        return Err(GitError::Command(
            "Working directory has unstaged changes".into(),
        ));
    }

    // diff-index --quiet --cached HEAD: refuse staged changes.
    if index_differs_from_head(git_dir, format)? {
        return Err(GitError::Command(
            "Working directory has staged changes".into(),
        ));
    }

    // Refuse untracked paths the new tree would overwrite (read-tree -u).
    if untracked_would_be_overwritten(worktree, git_dir, format, new_oid)? {
        return Err(GitError::Command(
            "Could not update working tree to new HEAD".into(),
        ));
    }

    sley_worktree::reset_index_and_worktree_to_commit(worktree, git_dir, format, new_oid).map_err(
        |err| {
            GitError::Command(format!(
                "Could not update working tree to new HEAD: {err}"
            ))
        },
    )?;
    Ok(())
}

fn index_differs_from_head(git_dir: &Path, format: sley_core::ObjectFormat) -> Result<bool> {
    use sley_index::Index;
    use sley_object::{Commit, ObjectType};
    use sley_odb::{FileObjectDatabase, ObjectReader};
    use sley_refs::{FileRefStore, RefTarget};

    let store = FileRefStore::new(git_dir, format);
    let head_oid = match store.read_ref("HEAD")? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        Some(RefTarget::Symbolic(target)) => match store.read_ref(&target)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
        None => None,
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&std::fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let Some(head_oid) = head_oid else {
        // Unborn HEAD: any non-empty index is a staged change.
        return Ok(!index.entries.is_empty());
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&head_oid)?;
    if object.object_type != ObjectType::Commit {
        return Ok(true);
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let mut index_entries = std::collections::BTreeMap::new();
    for entry in &index.entries {
        if entry.stage() != sley_index::Stage::Normal {
            return Ok(true);
        }
        index_entries.insert(entry.path.as_bytes().to_vec(), (entry.mode, entry.oid));
    }
    let mut tree_entries = std::collections::BTreeMap::new();
    flatten_tree_to_map(&db, format, &commit.tree, b"", &mut tree_entries)?;
    Ok(index_entries != tree_entries)
}

fn flatten_tree_to_map(
    db: &sley_odb::FileObjectDatabase,
    format: sley_core::ObjectFormat,
    tree_oid: &sley_core::ObjectId,
    prefix: &[u8],
    out: &mut std::collections::BTreeMap<Vec<u8>, (u32, sley_core::ObjectId)>,
) -> Result<()> {
    use sley_object::{ObjectType, TreeEntries};
    use sley_odb::ObjectReader;

    let object = match db.read_object(tree_oid) {
        Ok(object) => object,
        Err(_) => return Ok(()),
    };
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            flatten_tree_to_map(db, format, &entry.oid, &path, out)?;
        } else {
            out.insert(path, (entry.mode, entry.oid));
        }
    }
    Ok(())
}

fn untracked_would_be_overwritten(
    worktree: &Path,
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    new_oid: &sley_core::ObjectId,
) -> Result<bool> {
    use sley_index::Index;
    use sley_object::{Commit, ObjectType};
    use sley_odb::{FileObjectDatabase, ObjectReader};

    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(new_oid)?;
    if object.object_type != ObjectType::Commit {
        return Ok(false);
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let mut tree_paths = std::collections::BTreeMap::new();
    flatten_tree_to_map(&db, format, &commit.tree, b"", &mut tree_paths)?;

    let index_path = sley_worktree::repository_index_path(git_dir);
    let mut indexed = std::collections::BTreeSet::new();
    if index_path.exists() {
        let index = Index::parse(&std::fs::read(index_path)?, format)?;
        for entry in index.entries {
            indexed.insert(entry.path.as_bytes().to_vec());
        }
    }
    for path_bytes in tree_paths.keys() {
        if indexed.contains(path_bytes) {
            continue;
        }
        let rel = String::from_utf8_lossy(path_bytes);
        let abs = worktree.join(rel.as_ref());
        if abs.exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn find_hook_path(git_dir: &Path, hook_name: &str) -> Option<PathBuf> {
    let common = repository_common_dir(git_dir);
    let path = common.join("hooks").join(hook_name);
    if path.is_file() { Some(path) } else { None }
}

fn spawn_hook(
    git_dir: &Path,
    path: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    env: &[(String, String)],
    remote_stderr: &mut Vec<u8>,
    capture_stderr: bool,
) -> Result<()> {
    // git's receive-pack execs a hook by a path relative to the repo it chdir'd
    // into, so the hook's `$0` is `hooks/<name>` — not an absolute path
    // (t1416 #8 compares the update hook's `$0`). We already set the cwd to
    // git_dir, so strip that prefix; a path containing a slash still execs
    // relative to cwd rather than searching PATH.
    let exec_path = path.strip_prefix(git_dir).unwrap_or(path);
    let mut command = Command::new(exec_path);
    command
        .current_dir(git_dir)
        .env("GIT_DIR", git_dir)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // git's receive-pack runs every server hook with stdout_to_stderr=1 so
        // hook stdout reaches the client on the same channel as stderr
        // (t1800 #55/#56); it must never leak onto the push's stdout.
        .stdout(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::from(std::io::stderr())
        })
        .stderr(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|err| GitError::Io(format!("cannot spawn hook {}: {err}", path.display())))?;
    if let Some(input) = stdin
        && let Some(mut hook_stdin) = child.stdin.take()
    {
        let _ = hook_stdin.write_all(input);
    }
    let status = child.wait().map_err(|err| GitError::Io(err.to_string()))?;
    if capture_stderr {
        if let Some(mut stdout) = child.stdout.take() {
            let _ = std::io::copy(&mut stdout, remote_stderr);
        }
        if let Some(mut stderr) = child.stderr.take() {
            let _ = std::io::copy(&mut stderr, remote_stderr);
        }
    }
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

fn receive_hook_env(
    push_options: &[String],
    quarantine_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env = vec![(
        "GIT_PUSH_OPTION_COUNT".to_string(),
        push_options.len().to_string(),
    )];
    for (index, value) in push_options.iter().enumerate() {
        env.push((format!("GIT_PUSH_OPTION_{index}"), value.clone()));
    }
    env.extend_from_slice(quarantine_env);
    env
}

fn receive_hook_stdin(commands: &[ReceivePackCommand]) -> Vec<u8> {
    receive_stream_hook_order_commands(commands)
        .iter()
        .map(|command| format!("{} {} {}\n", command.old_id, command.new_id, command.name))
        .collect::<String>()
        .into_bytes()
}

fn post_receive_hook_stdin(commands: &[ReceivePackCommandState]) -> Vec<u8> {
    let mut out = Vec::new();
    for state in receive_stream_hook_order(commands) {
        if state.error_string.is_some() {
            continue;
        }
        let mut report_iter = state.reports.iter();
        let mut report = report_iter.next();
        loop {
            let (old_id, new_id, name) = if let Some(rep) = report {
                let old_id = rep.old_oid.as_ref().unwrap_or(&state.command.old_id);
                let new_id = rep.new_oid.as_ref().unwrap_or(&state.command.new_id);
                let name = rep
                    .refname
                    .as_deref()
                    .unwrap_or(state.command.name.as_str());
                (old_id, new_id, name)
            } else {
                (
                    &state.command.old_id,
                    &state.command.new_id,
                    state.command.name.as_str(),
                )
            };
            out.extend_from_slice(format!("{old_id} {new_id} {name}\n").as_bytes());
            report = report_iter.next();
            if report.is_none() {
                break;
            }
        }
    }
    out
}

fn receive_update_hook_order(commands: &[ReceivePackCommand]) -> Vec<&ReceivePackCommand> {
    let mut ordered = Vec::with_capacity(commands.len());
    ordered.extend(commands.iter().filter(|c| c.new_id.is_null()));
    ordered.extend(commands.iter().filter(|c| !c.new_id.is_null()));
    ordered
}

fn receive_stream_hook_order_commands(commands: &[ReceivePackCommand]) -> Vec<&ReceivePackCommand> {
    let mut existing: Vec<_> = commands
        .iter()
        .filter(|command| !command.old_id.is_null())
        .collect();
    existing.sort_by(|left, right| left.name.cmp(&right.name));
    existing.extend(commands.iter().filter(|command| command.old_id.is_null()));
    existing
}

fn receive_stream_hook_order(
    commands: &[ReceivePackCommandState],
) -> Vec<&ReceivePackCommandState> {
    let mut existing: Vec<_> = commands
        .iter()
        .filter(|state| !state.command.old_id.is_null())
        .collect();
    existing.sort_by(|left, right| left.command.name.cmp(&right.command.name));
    existing.extend(
        commands
            .iter()
            .filter(|state| state.command.old_id.is_null()),
    );
    existing
}
