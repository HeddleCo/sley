//! Traditional receive-pack hooks (`pre-receive`, `update`, `post-receive`, `post-update`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sley_core::{GitError, Result};
use sley_odb::repository_common_dir;
use sley_protocol::ReceivePackCommand;

use crate::proc_receive::ReceivePackCommandState;

pub fn hook_exists(git_dir: &Path, hook_name: &str) -> bool {
    find_hook_path(git_dir, hook_name).is_some()
}

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
    let mut command = Command::new(path);
    command
        .current_dir(git_dir)
        .env("GIT_DIR", git_dir)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
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
    if let Some(input) = stdin {
        if let Some(mut hook_stdin) = child.stdin.take() {
            let _ = hook_stdin.write_all(input);
        }
    }
    let status = child.wait().map_err(|err| GitError::Io(err.to_string()))?;
    if capture_stderr {
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
                let old_id = rep
                    .old_oid
                    .as_ref()
                    .unwrap_or(&state.command.old_id);
                let new_id = rep
                    .new_oid
                    .as_ref()
                    .unwrap_or(&state.command.new_id);
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

fn receive_update_hook_order<'a>(
    commands: &'a [ReceivePackCommand],
) -> Vec<&'a ReceivePackCommand> {
    let mut ordered = Vec::with_capacity(commands.len());
    ordered.extend(commands.iter().filter(|c| c.new_id.is_null()));
    ordered.extend(commands.iter().filter(|c| !c.new_id.is_null()));
    ordered
}

fn receive_stream_hook_order_commands<'a>(
    commands: &'a [ReceivePackCommand],
) -> Vec<&'a ReceivePackCommand> {
    let mut existing: Vec<_> = commands
        .iter()
        .filter(|command| !command.old_id.is_null())
        .collect();
    existing.sort_by(|left, right| left.name.cmp(&right.name));
    existing.extend(commands.iter().filter(|command| command.old_id.is_null()));
    existing
}

fn receive_stream_hook_order<'a>(
    commands: &'a [ReceivePackCommandState],
) -> Vec<&'a ReceivePackCommandState> {
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