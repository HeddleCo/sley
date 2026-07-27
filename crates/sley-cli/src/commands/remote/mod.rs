//! Remote command module tree (clone, fetch, push, ls-remote, `git remote`).

use crate::{GitError, Result};

mod admin;
mod clone;
mod config;
mod fetch;
mod helper;
mod http_backend;
mod ls_remote;
mod pack;
mod remote_curl;
mod resolve;

pub(crate) use admin::{
    cmd_remote, cmd_remote_add, cmd_remote_get_url, cmd_remote_prune, cmd_remote_remove,
    cmd_remote_rename, cmd_remote_set_branches, cmd_remote_set_head, cmd_remote_set_url,
    cmd_remote_show, cmd_remote_update,
};
pub(crate) use clone::{cmd_clone, parse_clone_depth};
pub(crate) use config::{
    read_effective_repo_config, read_repo_config, read_repo_config_on_disk, remote_exists,
    remote_names, repo_current_branch_name, validate_remote_name, write_repo_config,
};
pub(crate) use fetch::{
    FetchSubmoduleRequest, StdoutProgress, changed_gitlinks_for_fetch, cmd_fetch, fetch_bundle,
    fetch_git_repository, fetch_git_repository_with_outcome, fetch_http_repository_with_outcome,
    fetch_local_repository, fetch_local_repository_with_outcome,
    fetch_populated_submodules_after_superproject, fetch_ref_snapshot,
    fetch_set_upstream_from_outcome, fetch_source_is_git, fetch_source_is_http,
    fetch_source_is_ssh, fetch_ssh_repository, fetch_ssh_repository_with_outcome,
    resolve_fetch_recurse_submodules,
};
pub(crate) use helper::fetch_with_remote_helper;
pub(crate) use http_backend::cmd_http_backend;
pub(crate) use ls_remote::cmd_ls_remote;
pub(crate) use pack::{
    cmd_push, cmd_receive_pack, cmd_send_pack, cmd_upload_pack, probe_custom_local_upload_archive,
    probe_custom_local_upload_pack,
};
pub(crate) use remote_curl::cmd_remote_http;
pub(crate) use resolve::{RemoteCommandContext, ls_remote_git_dir};

pub(crate) const CLONE_UNBORN_BRANCH: &str = "__sley_clone_unborn__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchRecurseSubmodules {
    Default,
    OnDemand,
    On,
    Off,
}

impl FetchRecurseSubmodules {
    pub(crate) fn from_arg(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("yes") {
            "yes" | "true" | "on" => Ok(Self::On),
            "on-demand" => Ok(Self::OnDemand),
            "no" | "false" | "off" => Ok(Self::Off),
            other => {
                eprintln!("fatal: bad --recurse-submodules argument: {other}");
                Err(GitError::Exit(128))
            }
        }
    }

    pub(crate) fn from_config(value: &str) -> Self {
        match sley_submodule::parse_fetch_recurse(value) {
            sley_submodule::RecurseMode::On => Self::On,
            sley_submodule::RecurseMode::Off => Self::Off,
            sley_submodule::RecurseMode::OnDemand => Self::OnDemand,
            _ => Self::Default,
        }
    }
}
