#!/usr/bin/env python3
"""Split remote_cmds.rs into commands/remote/ module tree (line-range based)."""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "crates/sley-cli/src/commands/remote_cmds.rs"
OUT = ROOT / "crates/sley-cli/src/commands/remote"
OPTS_SRC = ROOT / "crates/sley-cli/src/commands/remote_ls_options.rs"

MOD_RS = """//! Remote command module tree (clone, fetch, push, ls-remote, `git remote`).

mod admin;
mod clone;
mod config;
mod fetch;
mod ls_remote;
mod ls_remote_options;
mod pack;
mod resolve;

pub(crate) use admin::{
    cmd_remote, cmd_remote_add, cmd_remote_get_url, cmd_remote_prune, cmd_remote_remove,
    cmd_remote_rename, cmd_remote_set_branches, cmd_remote_set_head, cmd_remote_set_url,
    cmd_remote_show, cmd_remote_update,
};
pub(crate) use clone::{cmd_clone, parse_clone_depth};
pub(crate) use config::{
    read_repo_config, read_repo_config_on_disk, remote_exists, remote_names,
    repo_current_branch_name, validate_remote_name, write_repo_config,
};
pub(crate) use fetch::{
    FetchRecurseSubmodules, FetchSubmoduleRequest, StdoutProgress, changed_gitlinks_for_fetch,
    fetch_bundle, fetch_local_repository, fetch_populated_submodules_after_superproject,
    fetch_ref_snapshot, fetch_set_upstream_from_outcome, fetch_source_is_git,
    fetch_source_is_ssh, fetch_git_repository, fetch_ssh_repository, pack_filter_from_spec,
    resolve_fetch_recurse_submodules, cmd_fetch,
};
pub(crate) use ls_remote::cmd_ls_remote;
pub(crate) use pack::{cmd_push, cmd_receive_pack, cmd_send_pack, cmd_upload_pack};
pub(crate) use resolve::ls_remote_git_dir;

pub(crate) const CLONE_UNBORN_BRANCH: &str = "__sley_clone_unborn__";
"""

ENUM_BLOCK = """pub(crate) enum FetchRecurseSubmodules {
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
"""

# 1-based inclusive line ranges from the original remote_cmds.rs.
RANGES: dict[str, list[tuple[int, int]]] = {
    # clone helpers through branch-merge helper used by fetch
    "clone": [(54, 3874)],
    "fetch": [
        (3875, 5585),  # default_fetch_remote .. before receive_max_input_size
        (6912, 6926),  # configured_server_options
        (9436, 9986),  # fetch_set_upstream .. fetch_http (before ls_remote_http_records)
        (10584, 10631),  # transport_policy_config_for_cwd, repo_config_with_transport_policy
    ],
    "pack": [
        (5586, 6911),  # receive_max_input_size .. before configured_server_options
        (6927, 9434),  # after configured_server_options .. configure_push_upstreams tail
        (12989, 13056),  # receive_max_input_size_tests
    ],
    "ls_remote": [
        (9987, 10583),  # ls_remote_http_records .. check_transport_allowed_url
        (10752, 10982),  # ls_remote_display_url .. print_ls_remote_ref
    ],
    "resolve": [
        (10633, 10750),  # ls_remote_git_dir .. percent_hex_value
        (12162, 12200),  # local_remote_git_dir .. repository_relative_path_base
    ],
    "admin": [
        (10985, 12161),  # cmd_remote .. discover_local_remote_head_branch
        (12202, 12858),  # cmd_remote_set_url .. local_branch_names
        (12925, 12939),  # remote_branch_fetch_refspec helpers
        (12972, 12987),  # validate_remote_branch_name
    ],
    "config": [
        (12860, 12896),  # read_repo_config .. clone_effective_config_value
        (12898, 12923),  # repo_current_branch_name .. remote_exists
        (12941, 12970),  # validate_remote_name
    ],
}

MODULE_HEADER = {
    "clone": "//! Clone command and helpers.",
    "fetch": "//! Fetch command, transport, and submodule recursion.",
    "pack": "//! receive-pack, upload-pack, send-pack, and push.",
    "ls_remote": "//! `git ls-remote` command and formatting.",
    "resolve": "//! Resolve a repository name/URL to a local git directory.",
    "admin": "//! `git remote` subcommands.",
    "config": "//! Repository config read/write and remote name helpers.",
}

MODULE_USES = {
    "clone": """use super::config::{read_repo_config, read_repo_config_on_disk, write_repo_config};
use super::fetch::{
    configured_server_options, fetch_bundle, fetch_source_is_git, fetch_source_is_ssh,
    parse_shallow_since, transport_policy_config_for_cwd,
};
use super::ls_remote::{check_transport_allowed_url, ls_remote_resolved_url};
use super::resolve::{local_repository_git_dir_path, ls_remote_git_dir};
use super::config::validate_remote_name;
use super::fetch::StdoutProgress;
use super::CLONE_UNBORN_BRANCH;
use crate::commands::config_cmd::{
    ConfigKey, SimpleConfigRegex, config_set_value, parse_config_key,
};
use crate::remote::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
use crate::*;
use sley_odb::ObjectReader;
use sley_remote::{FetchOptions, LsRemoteRecord};
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
""",
    "fetch": """use super::config::{read_repo_config, remote_exists, remote_names, write_repo_config};
use super::resolve::ls_remote_git_dir;
use crate::commands::config_cmd::{
    ConfigKey, SimpleConfigRegex, config_set_value, parse_config_key,
};
use crate::remote::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
use crate::*;
use sley_odb::ObjectReader;
use sley_remote::{FetchOptions, LsRemoteRecord};
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
""",
    "pack": """use super::config::{read_repo_config, write_repo_config};
use super::clone::{
    trace_index_pack_fsck_objects_if_configured, trace_pack_objects_filter,
};
use super::fetch::{
    configured_server_options, default_fetch_remote, transport_policy_config_for_cwd,
};
use super::resolve::ls_remote_git_dir;
use crate::commands::config_cmd::{
    ConfigKey, SimpleConfigRegex, config_set_value, parse_config_key,
};
use crate::remote::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
use crate::*;
use sley_odb::ObjectReader;
use sley_remote::{FetchOptions, LsRemoteRecord};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
""",
    "ls_remote": """use super::config::{read_repo_config, remote_exists};
use super::fetch::{
    configured_server_options, default_fetch_remote, transport_policy_config_for_cwd,
    repo_config_with_transport_policy,
};
use super::ls_remote_options::setup_ls_remote_options;
use super::resolve::ls_remote_git_dir;
use crate::commands::config_cmd::{
    ConfigKey, SimpleConfigRegex, config_set_value, parse_config_key,
};
use crate::remote::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
use crate::*;
use sley_odb::ObjectReader;
use sley_remote::{FetchOptions, LsRemoteRecord};
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
""",
    "resolve": """use super::config::{read_repo_config, remote_exists};
use crate::remote::{remote_config_values, rewrite_url_with_config};
use crate::*;
use std::path::{Path, PathBuf};
""",
    "admin": """use super::config::{
    read_repo_config, read_repo_config_on_disk, remote_exists, remote_names,
    validate_remote_name, write_repo_config,
};
use super::fetch::cmd_fetch;
use super::resolve::{local_remote_git_dir, ls_remote_git_dir};
use crate::commands::config_cmd::{
    ConfigKey, SimpleConfigRegex, config_set_value, parse_config_key,
};
use crate::remote::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
use crate::*;
use sley_odb::ObjectReader;
use sley_remote::{FetchOptions, LsRemoteRecord};
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
""",
    "config": """use crate::*;
use std::path::{Path, PathBuf};
""",
}

# Functions that must be visible to sibling modules.
PUB_SUPER: dict[str, set[str]] = {
    "fetch": {
        "configured_server_options",
        "transport_policy_config_for_cwd",
        "repo_config_with_transport_policy",
        "default_fetch_remote",
        "parse_shallow_since",
        "run_fetch",
        "fetch_local_repository_with_outcome",
        "fetch_http_repository_with_outcome",
        "fetch_ssh_repository_with_outcome",
        "fetch_git_repository_with_outcome",
        "fetch_http_repository",
        "fetch_source_is_http",
    },
    "resolve": {
        "local_remote_git_dir",
        "percent_decode_url_path",
        "local_repository_git_dir_path",
        "path_with_bundle_suffix",
    },
    "config": {"clone_effective_config_value"},
    "admin": {
        "remote_branch_fetch_refspec",
        "remote_add_fetch_refspec",
    },
    "pack": {
        "configured_protocol_version",
        "configured_legacy_protocol",
        "trace_configured_local_protocol_version",
        "trace_protocol_v2_ls_refs_request",
        "read_direct_or_symbolic_ref",
        "fetch_head_oid_for_push_lease",
    },
    "ls_remote": {
        "ls_remote_resolved_url",
        "check_transport_allowed_url",
    },
    "clone": {
        "parse_clone_config_override",
        "validate_local_clone_source_refs",
        "remote_head_detached",
        "path_with_bundle_suffix",
        "trace_index_pack_fsck_objects_if_configured",
        "trace_pack_objects_filter",
    },
}


def extract_ranges(lines: list[str], ranges: list[tuple[int, int]]) -> str:
    chunks: list[str] = []
    for start, end in sorted(ranges):
        chunks.extend(lines[start - 1 : end])
    return "".join(chunks)


def promote_pub_super(body: str, names: set[str]) -> str:
    for name in names:
        body = body.replace(f"\nfn {name}(", f"\npub(super) fn {name}(")
        body = body.replace(f"\npub(crate) fn {name}(", f"\npub(super) fn {name}(")
    return body


def fix_refs(body: str) -> str:
    return (
        body.replace(
            "super::pack::humanise_byte_count",
            "crate::commands::pack::humanise_byte_count",
        )
        .replace(
            "super::pack::fsck_pack_objects",
            "crate::commands::pack::fsck_pack_objects",
        )
    )


def main() -> None:
    lines = SRC.read_text().splitlines(keepends=True)
    OUT.mkdir(parents=True, exist_ok=True)

    (OUT / "mod.rs").write_text(MOD_RS + "\n" + ENUM_BLOCK + "\n")

    for module, ranges in RANGES.items():
        body = fix_refs(extract_ranges(lines, ranges))
        body = promote_pub_super(body, PUB_SUPER.get(module, set()))
        content = MODULE_HEADER[module] + "\n\n" + MODULE_USES[module] + "\n" + body
        (OUT / f"{module}.rs").write_text(content)
        print(f"{module}.rs: {content.count(chr(10))} lines")

    opts = OPTS_SRC.read_text()
    (OUT / "ls_remote_options.rs").write_text(
        "//! `git ls-remote` option parsing.\n\n"
        + opts.replace(
            "use super::{LsRemoteOptions, LsRemoteSort};",
            "use crate::{LsRemoteOptions, LsRemoteSort};",
        )
    )


if __name__ == "__main__":
    main()