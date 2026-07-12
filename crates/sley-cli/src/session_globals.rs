//! Session-scoped CLI globals (git-dir/work-tree overrides, pathspec magic, replace objects).

use std::collections::HashSet;
use std::env;

use sley::{ObjectId, ReferenceTarget as RefTarget, Result};
use sley_pathspec::{PathspecAttributeCheck, PathspecAttributeState};

use crate::sley_refs::FileRefStore;
use crate::sley_worktree;

/// Effective default pathspec magic, folding in the global options *and* the
/// `GIT_*_PATHSPECS` environment variables (git reads both). Literal magic
/// (`--literal-pathspecs`/`--noglob-pathspecs`/`GIT_LITERAL_PATHSPECS`/
/// `GIT_NOGLOB_PATHSPECS`) suppresses glob magic.
pub(crate) fn effective_pathspec_flags(
    cli_session: &crate::session::CliSession,
) -> sley_worktree::PathspecMatchMagic {
    let mut flags = cli_session.pathspec_flags();
    if git_env_bool("GIT_LITERAL_PATHSPECS") {
        flags.literal = true;
        flags.literal_pathspecs = true;
    }
    if git_env_bool("GIT_NOGLOB_PATHSPECS") {
        flags.literal = true;
    }
    if git_env_bool("GIT_GLOB_PATHSPECS") {
        flags.glob = true;
    }
    if git_env_bool("GIT_ICASE_PATHSPECS") {
        flags.icase = true;
    }
    sley_worktree::PathspecMatchMagic {
        literal: flags.literal,
        glob: flags.glob && !flags.literal && !flags.literal_pathspecs,
        icase: flags.icase,
        literal_pathspecs: flags.literal_pathspecs,
    }
}

pub(crate) fn attribute_checks_for_matching(
    checks: Vec<sley_worktree::AttributeCheck>,
) -> Vec<PathspecAttributeCheck> {
    checks
        .into_iter()
        .map(|check| PathspecAttributeCheck {
            attribute: check.attribute,
            state: check.state.map(|state| match state {
                sley_worktree::AttributeState::Set => PathspecAttributeState::Set,
                sley_worktree::AttributeState::Unset => PathspecAttributeState::Unset,
                sley_worktree::AttributeState::Value(value) => PathspecAttributeState::Value(value),
            }),
        })
        .collect()
}

pub(crate) fn git_env_bool(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => !matches!(value.as_str(), "" | "0" | "false" | "no" | "off"),
        Err(_) => false,
    }
}

pub(crate) fn global_attr_source(cli_session: &crate::session::CliSession) -> Option<String> {
    cli_session.attr_source()
}

pub(crate) fn apply_replace_object(
    replace_objects: bool,
    refs: &FileRefStore,
    oid: &ObjectId,
) -> Result<ObjectId> {
    if !replace_objects {
        return Ok(*oid);
    }
    let mut current = *oid;
    let mut seen = HashSet::new();
    for _ in 0..5 {
        if !seen.insert(current) {
            break;
        }
        let name = format!("refs/replace/{current}");
        match refs.read_ref(&name)? {
            Some(RefTarget::Direct(next)) => current = next,
            _ => break,
        }
    }
    Ok(current)
}
