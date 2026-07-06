//! Shared helpers for engine parity modules.

use std::path::PathBuf;

use sley::{Index, Repository};
use sley::plumbing::sley_worktree::{update_index_paths, UpdateIndexOptions};
use sley_testkit::engine_parity::{format_index_stage_lines, EngineOutput};

/// Run `update_index_paths` for `paths` and return the resulting staged index.
pub fn run_update_index(
    repo: &Repository,
    paths: &[&str],
    options: UpdateIndexOptions,
) -> EngineOutput {
    let workdir = repo.workdir().expect("worktree required for update-index");
    let path_bufs: Vec<PathBuf> = paths.iter().map(|p| PathBuf::from(*p)).collect();
    update_index_paths(
        &workdir,
        repo.git_dir(),
        repo.object_format(),
        &path_bufs,
        options,
    )
    .expect("update_index_paths");
    index_stage_output(repo)
}

/// Format the repository index the way `git ls-files --stage` prints it.
pub fn index_stage_output(repo: &Repository) -> EngineOutput {
    let index = repo.read_index().expect("read index");
    EngineOutput::stdout(format_index_stage(&index))
}

pub fn format_index_stage(index: &Index) -> Vec<u8> {
    format_index_stage_lines(&index.entries)
}