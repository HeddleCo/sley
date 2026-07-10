//! Session-scoped CLI globals (git-dir/work-tree overrides, pathspec magic, replace objects).

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;

use sley::{ObjectId, ReferenceTarget as RefTarget, Result};
use sley_pathspec::{PathspecAttributeCheck, PathspecAttributeState};

use crate::session;
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

fn global_git_dir() -> Option<PathBuf> {
    session::cli_session().and_then(|session| session.git_dir_override())
}

fn global_work_tree() -> Option<PathBuf> {
    session::cli_session().and_then(|session| session.work_tree_override())
}

pub(crate) fn global_attr_source(cli_session: &crate::session::CliSession) -> Option<String> {
    cli_session.attr_source()
}

pub(crate) fn environment_git_dir() -> Option<PathBuf> {
    if local_repo_env_hidden() {
        return None;
    }
    env::var_os("GIT_DIR").map(PathBuf::from)
}

pub(crate) fn explicit_git_dir() -> Option<PathBuf> {
    global_git_dir().or_else(environment_git_dir)
}

fn environment_work_tree() -> Option<PathBuf> {
    if local_repo_env_hidden() {
        return None;
    }
    env::var_os("GIT_WORK_TREE").map(PathBuf::from)
}

pub(crate) fn explicit_work_tree() -> Option<PathBuf> {
    global_work_tree().or_else(environment_work_tree)
}

pub(crate) fn global_bare() -> bool {
    let Some(session) = session::cli_session() else {
        return false;
    };
    if session.local_repo_env_hidden() {
        return false;
    }
    session.bare()
}

fn local_repo_env_hidden() -> bool {
    session::cli_session().is_some_and(|session| session.local_repo_env_hidden())
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
