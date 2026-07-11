//! Repository config read/write and remote name helpers.

use crate::*;
use sley::plumbing::{sley_config, sley_refs};
use std::path::{Path, PathBuf};

pub(crate) fn read_repo_config(git_dir: &Path) -> Result<GitConfig> {
    // Single effective-config reader shared with the library crates: resolves
    // `include.path` / `includeIf` and layers command-line `-c` / `--config-env`
    // / `GIT_CONFIG_*` overrides on top (highest precedence). git applies these to
    // all config reads, not just `git config`, so consumers like `git log`'s
    // i18n.* lookups must see them. The CLI holds command-line `-c` overrides it
    // cannot push into the process env, so it reconstructs the effective
    // `GIT_CONFIG_PARAMETERS` and passes it through.
    sley_config::read_repo_config(git_dir, crate::effective_config_parameters_env().as_deref())
}

/// Read the complete invocation config in Git precedence order: system,
/// global, repository, then command-scoped injections. This is the config view
/// for command behavior; read-modify-write callers must continue to use
/// [`read_repo_config_on_disk`] so inherited settings are never persisted.
pub(crate) fn read_effective_repo_config(git_dir: &Path, cwd: &Path) -> Result<GitConfig> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let context = sley_config::ConfigIncludeContext::new(
        Some(sley_config::git_dir_for_include_context(git_dir)),
        repo_current_branch_name(git_dir),
    );
    let mut config = sley_config::load_effective_config(&common_git_dir, &context)?;
    if config
        .get_bool("extensions", None, "worktreeconfig")
        .unwrap_or(false)
    {
        let worktree =
            sley_config::load_config_with_includes(&git_dir.join("config.worktree"), &context)?;
        config.sections.extend(worktree.sections);
    }
    let parameters = injected_config_parameters()?;
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        cwd,
    )?;
    sley_config::remotes::augment_with_legacy_remote_files(&mut config, git_dir);
    Ok(config)
}

/// The repository's on-disk `config` file alone, with NO command-line `-c` /
/// `GIT_CONFIG_*` injection layered on. Use this for the read side of any
/// read-modify-write that persists the result back to the config file:
/// [`read_repo_config`] folds the process-level injection into the returned
/// config, so writing it back would persist `git -c key=value` into the file
/// (upstream keeps `-c` injections process-local and never writes them out).
/// This is the bug class behind clone wrongly baking `git -c …` into the cloned
/// repo's config. Includes (`include.path` / `includeIf`) are still resolved.
pub(crate) fn read_repo_config_on_disk(git_dir: &Path) -> Result<GitConfig> {
    sley_config::read_repo_config(git_dir, None)
}

/// A single `<section>.<key>` value from the *full effective config* (system +
/// global + repository, includes resolved) for `git_dir`. Unlike
/// [`read_repo_config`] — which reads only the repo's own `config` file — this
/// layers the global `~/.gitconfig` and system files, as git does for settings
/// like `branch.autosetuprebase` that are configured outside the cloned repo.
pub(super) fn clone_effective_config_value(
    git_dir: &Path,
    section: &str,
    key: &str,
) -> Option<String> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir).ok()?;
    let context = sley_config::ConfigIncludeContext::new(
        Some(common_git_dir.clone()),
        repo_current_branch_name(git_dir),
    );
    let config = sley_config::load_effective_config(&common_git_dir, &context).ok()?;
    config.get(section, None, key).map(str::to_owned)
}
/// Short branch name from `HEAD` (e.g. "main"), or None when detached/unborn.
/// Used for `includeIf "onbranch:<glob>"` resolution; reads HEAD directly so it
/// needs no object-format or ref-store context.
pub(crate) fn repo_current_branch_name(git_dir: &Path) -> Option<String> {
    sley_config::repo_current_branch_name(git_dir)
}

pub(crate) fn write_repo_config(git_dir: &Path, config: &GitConfig) -> Result<()> {
    if git_dir.join("config.lock").exists() {
        eprintln!(
            "error: could not lock config file {}: File exists",
            git_dir.join("config").display()
        );
        return Err(GitError::Exit(255));
    }
    fs::write(git_dir.join("config"), config.to_canonical_bytes())?;
    Ok(())
}

pub(crate) fn remote_names(config: &GitConfig) -> Vec<String> {
    sley_config::remotes::remote_names(config)
}

pub(crate) fn remote_exists(config: &GitConfig, name: &str) -> bool {
    sley_config::remotes::remote_exists(config, name)
}
pub(crate) fn validate_remote_name(name: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('-') {
        return Err(GitError::InvalidFormat("remote name is invalid".into()));
    }
    if name.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "remote name contains a delimiter byte".into(),
        ));
    }
    // git's `valid_remote_name` (remote.c) builds the fetch refspec
    // `refs/heads/test:refs/remotes/<name>/test` and rejects the name if that is
    // not a valid fetch refspec — this catches names with a colon, control
    // chars, or other refname-invalid spellings (e.g. `some:url`). The refspec
    // parser only screens delimiter bytes, so apply git's full
    // `check_refname_format` to the destination ref the name produces, matching
    // upstream's `valid_fetch_refspec` (which runs `check_refname_format` on the
    // refspec ends): this rejects `..` (e.g. `invalid...name`), trailing dots,
    // and `@{` that the delimiter screen lets through.
    let probe = format!("refs/heads/test:refs/remotes/{name}/test");
    let probe_dst = format!("refs/remotes/{name}/test");
    if sley_protocol::parse_refspec(&probe).is_err()
        || sley_refs::check_refname_format(&probe_dst, false).is_err()
    {
        // Upstream `builtin/remote.c` (add / rename): `die("'%s' is not a valid
        // remote name")` — a `fatal:` line and exit 128.
        eprintln!("fatal: '{name}' is not a valid remote name");
        return Err(GitError::Exit(128));
    }
    Ok(())
}
