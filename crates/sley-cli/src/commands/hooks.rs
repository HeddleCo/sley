//! Thin CLI wrapper over [`sley::hooks`].

use crate::session::CliSession;
use crate::{GitError, ObjectFormat, ObjectId, Result};
pub(crate) use sley::hooks::{
    HookRun, KNOWN_HOOKS, run_reference_transaction_hook_at, run_traditional_hook_at,
};
use sley::plumbing::sley_config::GitConfig;
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};

fn hook_environment(cli_session: &CliSession) -> Result<sley::hooks::HookEnvironment> {
    let git_dir = cli_session.git_dir()?;
    Ok(hook_environment_at(Some(&git_dir)))
}

/// Like [`hook_environment`], but allow out-of-repo invocations so
/// `git hook list|run` can still execute global configured hooks (t1800).
fn hook_environment_optional(cli_session: &CliSession) -> sley::hooks::HookEnvironment {
    let git_dir = cli_session.git_dir().ok();
    hook_environment_at(git_dir.as_deref())
}

fn hook_environment_at(git_dir: Option<&Path>) -> sley::hooks::HookEnvironment {
    sley::hooks::HookEnvironment {
        injected_config: crate::injected_config_parameters().ok(),
        git_dir: git_dir.map(Path::to_path_buf),
    }
}

pub(crate) fn run_hook_at(git_dir: &Path, hook_name: &str, options: HookRun) -> Result<bool> {
    sley::hooks::run_hook(hook_name, options, &hook_environment_at(Some(git_dir)))
}

pub(crate) fn run_hook(
    cli_session: &CliSession,
    hook_name: &str,
    options: HookRun,
) -> Result<bool> {
    sley::hooks::run_hook(hook_name, options, &hook_environment(cli_session)?)
}

pub(crate) fn run_hook_l_at(git_dir: &Path, hook_name: &str, args: &[&str]) -> Result<bool> {
    sley::hooks::run_hook_l(hook_name, args, &hook_environment_at(Some(git_dir)))
}

pub(crate) fn cmd_hook(cli_session: &CliSession, args: &[String]) -> Result<()> {
    // `git hook list|run` must work outside a repository so global hooks
    // configured via `hook.<name>.event` / `.command` still execute (t1800).
    sley::hooks::cmd_hook_with_env(args, &hook_environment_optional(cli_session))
}

pub(crate) fn run_post_index_change_hook_at(
    git_dir: &Path,
    updated_workdir: bool,
    updated_skipworktree: bool,
) -> Result<bool> {
    sley::hooks::run_post_index_change_hook(
        updated_workdir,
        updated_skipworktree,
        &hook_environment_at(Some(git_dir)),
    )
}

pub(crate) fn run_hook_l(cli_session: &CliSession, hook_name: &str, args: &[&str]) -> Result<bool> {
    sley::hooks::run_hook_l(hook_name, args, &hook_environment(cli_session)?)
}

pub(crate) fn hook_exists(cli_session: &CliSession, hook_name: &str) -> Result<bool> {
    sley::hooks::hook_exists(hook_name, &hook_environment(cli_session)?)
}

pub(crate) fn run_post_index_change_hook(
    cli_session: &CliSession,
    updated_workdir: bool,
    updated_skipworktree: bool,
) -> Result<bool> {
    sley::hooks::run_post_index_change_hook(
        updated_workdir,
        updated_skipworktree,
        &hook_environment(cli_session)?,
    )
}

/// Run every configured `gc.recentObjectsHook` command and return the object
/// ids it printed, preserving config and output order.
///
/// Unlike traditional hooks these are command strings, may be multi-valued,
/// and are executed by the shell from the repository invocation directory.
/// Any failing command aborts the caller before it mutates object storage.
pub(crate) fn run_recent_objects_hooks(
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
            .stderr(Stdio::inherit())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_object_hooks_preserve_multiple_values_and_output_order() {
        let first = "1111111111111111111111111111111111111111";
        let second = "2222222222222222222222222222222222222222";
        let config = GitConfig::parse(
            format!(
                "[gc]\n\trecentObjectsHook = printf '{first}\\n'\n\trecentObjectsHook = printf '{second}\\n'\n"
            )
            .as_bytes(),
        )
        .expect("parse config");
        let roots = run_recent_objects_hooks(&config, ObjectFormat::Sha1, Path::new("."))
            .expect("run hooks");
        assert_eq!(
            roots.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
            [first, second]
        );
    }

    #[test]
    fn recent_object_hook_failure_aborts_enumeration() {
        let config =
            GitConfig::parse(b"[gc]\n\trecentObjectsHook = false\n").expect("parse config");
        assert!(matches!(
            run_recent_objects_hooks(&config, ObjectFormat::Sha1, Path::new(".")),
            Err(GitError::Exit(128))
        ));
    }
}
