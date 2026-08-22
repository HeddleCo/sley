//! Commit message assembly, cleanup modes, and related validation helpers.
//!
//! The canonical implementations moved to `sley_sequencer::commit_message`;
//! the re-exports keep every historical crate-root name working across command
//! modules with no per-site edits. Only the session-aware ODB opening for
//! reused-commit lookups stays here (replacement policy comes from the
//! invocation session/config).

pub(crate) use sley::plumbing::sley_sequencer::commit_message::{
    CommitCleanupMode, commit_cleanup_message,
    commit_inter_hunk_context_expects_numerical_value_error,
    commit_inter_hunk_context_requires_value_error, commit_locate_scissors,
    commit_message_from_prepared_chunks, commit_message_requires_value_error,
    commit_stripspace_message, commit_tree_file_requires_value_error,
    commit_unified_expects_numerical_value_error, commit_unified_requires_value_error,
    patch_validate_inter_hunk_context, patch_validate_unified_context, read_commit_message_file,
    read_commit_pathspecs_from_file, resolve_commit_cleanup_mode,
};

use std::path::Path;

use sley::{ObjectFormat, Result};
use sley::plumbing::sley_object::Commit;
use sley::plumbing::sley_sequencer::commit_message::read_reused_commit_from_db;

use crate::repository::open_object_database;

pub(crate) fn read_reused_commit(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    replace_objects: bool,
) -> Result<Commit> {
    let db = open_object_database(git_dir, format, replace_objects)?;
    read_reused_commit_from_db(git_dir, format, &db, rev)
}
