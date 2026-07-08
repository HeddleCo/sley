//! Thin CLI wrapper over [`sley::hooks`].

use crate::Result;
pub(crate) use sley::hooks::{
    HookRun, KNOWN_HOOKS, run_reference_transaction_hook_at, run_traditional_hook_at,
};

fn hook_environment() -> sley::hooks::HookEnvironment {
    sley::hooks::HookEnvironment {
        injected_config: crate::injected_config_parameters().ok(),
        git_dir: crate::session::cli_git_dir().ok(),
    }
}

pub(crate) fn run_hook(hook_name: &str, options: HookRun) -> Result<bool> {
    sley::hooks::run_hook(hook_name, options, &hook_environment())
}

pub(crate) fn cmd_hook(args: &[String]) -> Result<()> {
    sley::hooks::cmd_hook_with_env(args, &hook_environment())
}

pub(crate) fn run_hook_l(hook_name: &str, args: &[&str]) -> Result<bool> {
    sley::hooks::run_hook_l(hook_name, args, &hook_environment())
}

pub(crate) fn hook_exists(hook_name: &str) -> Result<bool> {
    sley::hooks::hook_exists(hook_name, &hook_environment())
}

pub(crate) fn run_post_index_change_hook(
    updated_workdir: bool,
    updated_skipworktree: bool,
) -> Result<bool> {
    sley::hooks::run_post_index_change_hook(
        updated_workdir,
        updated_skipworktree,
        &hook_environment(),
    )
}
