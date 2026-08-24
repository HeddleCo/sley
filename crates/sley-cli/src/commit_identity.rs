//! Identity resolution canonical implementations live in
//! `sley_object::identity` (the env/config precedence chain) and
//! `sley_config::load_identity_effective_config` (effective-snapshot assembly).
//! The re-exports below keep every historical crate-root name working across
//! command modules with no per-site edits; this module hosts the one genuinely
//! session-bound piece: turning an invocation session into repository paths.

pub(crate) use sley::plumbing::sley_object::{
    IdentityConfig, canonicalize_commit_date, commit_identity_from_env,
    commit_identity_from_env_with_date, commit_reflog_message, commit_reflog_message_with_initial,
    commit_signoff_from_env, committer_identity_for_reflog, default_committer,
    identity_config_value, identity_config_value_for_role, identity_default_value,
    identity_use_config_only_error, try_canonicalize_commit_date, validate_commit_identity_name,
};

use sley::GitConfig;

use crate::common_git_dir_for_git_dir;
use crate::session;
use crate::sley_config;

/// Load identity/config fallback using an explicit invocation session.
pub(crate) fn identity_effective_config_for(
    cli_session: &session::CliSession,
) -> Option<GitConfig> {
    let git_dir = cli_session.git_dir().ok()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir).ok()?;
    sley_config::load_identity_effective_config(&common_git_dir, &git_dir, cli_session.cwd())
}
