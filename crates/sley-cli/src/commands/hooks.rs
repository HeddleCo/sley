//! Thin CLI wrapper over [`sley::hooks`].

use crate::Result;
use crate::session::CliSession;
pub(crate) use sley::hooks::{
    HookRun, KNOWN_HOOKS, run_reference_transaction_hook_at, run_traditional_hook_at,
};
use std::path::Path;

fn hook_environment(cli_session: &CliSession) -> Result<sley::hooks::HookEnvironment> {
    let git_dir = cli_session.git_dir()?;
    Ok(hook_environment_at(Some(&git_dir)))
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
    sley::hooks::cmd_hook_with_env(args, &hook_environment(cli_session)?)
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
