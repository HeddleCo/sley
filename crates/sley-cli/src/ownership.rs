//! Repository ownership validation (`safe.directory`), a port of git's
//! `ensure_valid_ownership` / `safe_directory_cb` from `setup.c`.
//!
//! When a repository's worktree / git directory is not owned by the current
//! user, git refuses to operate on it ("detected dubious ownership") unless the
//! path has been allow-listed via the `safe.directory` *protected* config
//! (system + global + command-line `-c` / `GIT_CONFIG_*` env — never the repo's
//! own config). `GIT_TEST_ASSUME_DIFFERENT_OWNER=1` forces the not-owned path so
//! the check can be exercised regardless of real on-disk ownership.

use crate::sley_config;
use sley::plumbing::sley_core;
use std::path::{Component, Path, PathBuf};

use sley::plumbing::sley_config::ConfigIncludeContext;
use sley::{GitError, Result};

use crate::{git_env_bool, injected_config_parameters};

/// Validate that operating on a repository is safe, mirroring git's
/// `ensure_valid_ownership`. `worktree` is the worktree top (`None` for a bare
/// repo), `gitdir` the resolved git directory, and `gitfile` the `.git` *file*
/// path when discovery went through a gitfile. Returns the dubious-ownership
/// fatal error when the repository is neither owned nor allow-listed.
pub(crate) fn ensure_valid_ownership(
    worktree: Option<&Path>,
    gitdir: &Path,
    gitfile: Option<&Path>,
) -> Result<()> {
    if is_valid_ownership(worktree, gitdir, gitfile) {
        return Ok(());
    }
    // The reported path is the worktree (non-bare) or git dir (bare); git uses
    // the gitfile when present, else the git dir.
    let reported = gitfile.unwrap_or(gitdir);
    Err(dubious_ownership_error(reported))
}

/// git's `ensure_valid_ownership` predicate: owned (and not forced different),
/// or the identifying path is allow-listed by `safe.directory`.
fn is_valid_ownership(worktree: Option<&Path>, gitdir: &Path, gitfile: Option<&Path>) -> bool {
    if !git_env_bool("GIT_TEST_ASSUME_DIFFERENT_OWNER")
        && gitfile.is_none_or(path_owned_by_current_user)
        && worktree.is_none_or(path_owned_by_current_user)
        && path_owned_by_current_user(gitdir)
    {
        return true;
    }
    // The repository is identified by its worktree (or git dir, when bare).
    let identity = worktree.unwrap_or(gitdir);
    let Some(normalized) = real_pathdup(identity) else {
        return false;
    };
    is_safe_directory(&normalized)
}

/// git's `safe.bareRepository` policy: whether bare repositories may be used
/// when discovered implicitly (by walking up), or only when named explicitly via
/// `GIT_DIR` / `--git-dir`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AllowedBareRepo {
    All,
    Explicit,
}

/// Record that an implicitly-discovered bare repository is being used (git's
/// `trace2_data_string("setup", "implicit-bare-repository", dir)`), then enforce
/// the `safe.bareRepository` policy: in `explicit` mode an implicit bare repo
/// that is not a known git-internal directory (`.git`, a secondary worktree, or
/// a submodule git dir) is refused.
pub(crate) fn note_implicit_bare_repository(dir: &Path) -> Result<()> {
    sley_core::trace2::perf_setup_data("implicit-bare-repository", dir.display());
    if get_allowed_bare_repo() == AllowedBareRepo::Explicit && !is_implicit_bare_repo(dir) {
        return Err(GitError::InvalidFormat(format!(
            "fatal: cannot use bare repository '{}' (safe.bareRepository is 'explicit')",
            dir.display()
        )));
    }
    Ok(())
}

/// Read `safe.bareRepository` from the protected config (git's
/// `get_allowed_bare_repo`); the default is `all`. The last valid value wins.
fn get_allowed_bare_repo() -> AllowedBareRepo {
    let context = ConfigIncludeContext::new(None, None);
    let Ok(mut config) = sley_config::load_pre_dispatch_config(None, &context) else {
        return AllowedBareRepo::All;
    };
    if let Ok(parameters) = injected_config_parameters() {
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let _ = sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            &base,
        );
    }
    let mut allowed = AllowedBareRepo::All;
    for value in config.get_all("safe", None, "bareRepository") {
        match value {
            Some("explicit") => allowed = AllowedBareRepo::Explicit,
            Some("all") => allowed = AllowedBareRepo::All,
            _ => {}
        }
    }
    allowed
}

/// git's `is_implicit_bare_repo`: a bare-looking directory that is actually a
/// known git-internal location — a `.git` directory, a secondary worktree's git
/// dir, or a submodule's git dir — and therefore exempt from the
/// `safe.bareRepository=explicit` refusal.
fn is_implicit_bare_repo(path: &Path) -> bool {
    if path.file_name().is_some_and(|name| name == ".git") {
        return true;
    }
    let text = path.to_string_lossy();
    text.contains("/.git/worktrees/") || text.contains("/.git/modules/")
}

/// git's dubious-ownership `die()` message, as a `fatal:`-prefixed
/// [`GitError::InvalidFormat`] so it propagates (and prints) only when the
/// error reaches the top level — not when a gentle prober swallows it.
fn dubious_ownership_error(path: &Path) -> GitError {
    let display = path.display();
    GitError::InvalidFormat(format!(
        "fatal: detected dubious ownership in repository at '{display}'\n\
         To add an exception for this directory, call:\n\
         \n\
         \tgit config --global --add safe.directory {display}"
    ))
}

/// Whether the configured `safe.directory` entries allow `normalized` (already
/// real-path'd). Mirrors `safe_directory_cb`'s reset / `*` / glob / exact logic,
/// processing the *protected* config in precedence order so a later empty value
/// resets the decision.
fn is_safe_directory(normalized: &Path) -> bool {
    let mut is_safe = false;
    for value in protected_safe_directory_values() {
        if value.is_empty() {
            is_safe = false;
        } else if value == "*" {
            is_safe = true;
        } else {
            let allowed = expand_tilde(&value);
            // A non-absolute entry is meaningless except for ".", which means
            // "only if we are at the repository top".
            if !Path::new(&allowed).is_absolute() && allowed != "." {
                continue;
            }
            let Some(entry) = real_pathdup(Path::new(&allowed)) else {
                continue;
            };
            if let Some(prefix) = entry.to_string_lossy().strip_suffix('*') {
                // `<dir>/*`: prefix match against everything but the trailing `*`.
                if normalized.to_string_lossy().starts_with(prefix) {
                    is_safe = true;
                }
            } else if entry == normalized {
                is_safe = true;
            }
        }
    }
    is_safe
}

/// The ordered `safe.directory` values from the protected config (system +
/// global files plus `-c` / `GIT_CONFIG_*` injected parameters), lowest
/// precedence first. The repository's own config is deliberately excluded.
fn protected_safe_directory_values() -> Vec<String> {
    let context = ConfigIncludeContext::new(None, None);
    // `load_pre_dispatch_config(None, ..)` reads system + global (with includes)
    // but no repository config — exactly git's protected scopes minus `command`.
    let Ok(mut config) = sley_config::load_pre_dispatch_config(None, &context) else {
        return Vec::new();
    };
    if let Ok(parameters) = injected_config_parameters() {
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let _ = sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            &base,
        );
    }
    config
        .get_all("safe", None, "directory")
        .into_iter()
        .map(|value| value.unwrap_or("").to_string())
        .collect()
}

/// git's `is_path_owned_by_current_user`: true when the path is owned by the
/// current user. The current uid is read (without an `unsafe` `getuid`) from
/// `/proc/self`'s owner; when it can't be determined the path is treated as
/// owned, so ownership enforcement only ever activates where it can be trusted
/// (and the parity tests drive it via `GIT_TEST_ASSUME_DIFFERENT_OWNER`).
#[cfg(unix)]
fn path_owned_by_current_user(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(uid) = current_uid() else {
        return true;
    };
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.uid() == uid,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn path_owned_by_current_user(_path: &Path) -> bool {
    true
}

/// The current process's real uid, read from `/proc/self`'s owner.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

/// git's `real_pathdup(path, 0)`: resolve the longest existing prefix of `path`
/// (following symlinks) and re-append any non-existent trailing components, so a
/// `safe.directory` glob like `/repos/*` normalizes to `realpath(/repos)/*`.
/// Returns `None` when even the path's existing ancestor can't be resolved.
fn real_pathdup(path: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some(resolved);
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path;
    loop {
        let parent = current.parent()?;
        if let Some(name) = current.file_name() {
            tail.push(name.to_os_string());
        }
        if parent.as_os_str().is_empty() {
            // A relative path whose first component doesn't exist: anchor it at
            // the lexical absolutization of the cwd.
            let base = std::env::current_dir().ok()?;
            return Some(append_tail(
                lexical_normalize(&base.join(path)),
                &tail,
                false,
            ));
        }
        if let Ok(resolved) = std::fs::canonicalize(parent) {
            return Some(append_tail(resolved, &tail, true));
        }
        current = parent;
    }
}

/// Append the collected (reversed) trailing components onto a resolved base.
fn append_tail(mut base: PathBuf, tail: &[std::ffi::OsString], reversed: bool) -> PathBuf {
    if reversed {
        for component in tail.iter().rev() {
            base.push(component);
        }
    }
    base
}

/// Drop `.`/`..` lexically from an absolute path (no filesystem access).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Minimal `~` / `~/...` expansion for `safe.directory` values (git's
/// `git_config_pathname`). `~user` is left untouched (unsupported, like the
/// tests, which only use absolute paths and ".").
fn expand_tilde(value: &str) -> String {
    if value == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return home.to_string_lossy().into_owned();
        }
    } else if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    value.to_string()
}
