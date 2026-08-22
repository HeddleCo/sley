//! Local repository copy: clone a checkout or bare repository into a bare
//! destination without invoking transport helpers.
//!
//! This is the no-git-runtime fast path for local sources: objects move via a
//! pack transfer ([`Repository::copy_reachable_from`]), refs are applied in one
//! atomic ref transaction, and the destination's `HEAD` mirrors the source's
//! checked-out branch instead of blindly pointing at `main`. Network clones
//! belong to [`crate::remote::clone_repository`]; this module is the
//! same-filesystem counterpart.

use std::collections::HashSet;
use std::path::Path;

use sley_refs::{RefTarget, ReflogEntry};

use crate::{FullName, GitError, HeadUpdateOptions, ObjectId, RefChange, Repository, Result};

/// Options for [`clone_local_to_bare`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCloneOptions {
    reflog_message: Option<String>,
}

impl LocalCloneOptions {
    pub fn new() -> Self {
        Self {
            reflog_message: None,
        }
    }

    /// Reflog message recorded on every copied branch. Defaults to
    /// `clone: copied from <source path>`.
    pub fn reflog_message(mut self, message: impl Into<String>) -> Self {
        self.reflog_message = Some(message.into());
        self
    }

    pub fn get_reflog_message(&self) -> Option<&str> {
        self.reflog_message.as_deref()
    }
}

impl Default for LocalCloneOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`clone_local_to_bare`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCloneSummary {
    /// Number of direct refs copied from the source.
    pub refs_copied: usize,
    /// The branch the destination's `HEAD` was pointed at, when any branch was
    /// copied. `None` leaves `HEAD` at its initialized default.
    pub head_branch: Option<String>,
}

/// Copy a local Git repository into a bare repository without spawning Git or
/// Sley transport helpers.
///
/// * `source` is discovered like a user-supplied working-tree path (falling
///   back to treating it as a git directory).
/// * `dest` is opened as an existing git directory or initialized as a new
///   bare repository; existing refs are updated in place (a re-copy merges).
/// * Every direct ref under `refs/` is copied; symbolic refs other than the
///   mirrored `HEAD` choice are not.
/// * Objects are transferred before refs, so a failed copy never leaves the
///   destination with dangling branch tips.
pub fn clone_local_to_bare(
    source: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    options: &LocalCloneOptions,
) -> Result<LocalCloneSummary> {
    let source_path = source.as_ref();
    let dest_path = dest.as_ref();
    let source_repo = open_source(source_path)?;
    std::fs::create_dir_all(dest_path)
        .map_err(|err| GitError::Io(format!("create {}: {err}", dest_path.display())))?;
    let target = match Repository::open(dest_path) {
        Ok(repo) => repo,
        Err(_) => Repository::init_bare(dest_path)?,
    };

    let updates: Vec<(String, ObjectId)> = source_repo
        .references()
        .list_refs()?
        .into_iter()
        .filter_map(|reference| match reference.target {
            RefTarget::Direct(oid) => Some((reference.name, oid)),
            RefTarget::Symbolic(_) => None,
        })
        .collect();

    if updates.is_empty() {
        return Ok(LocalCloneSummary {
            refs_copied: 0,
            head_branch: None,
        });
    }

    // Objects first: refs must never point into a missing object graph.
    let roots: Vec<ObjectId> = updates.iter().map(|(_, oid)| *oid).collect();
    target.copy_reachable_from(&source_repo, &roots)?;

    apply_copied_refs(&target, &updates, source_path, options)?;
    let head_branch = mirror_source_head(&source_repo, &target, &updates)?;
    Ok(LocalCloneSummary {
        refs_copied: updates.len(),
        head_branch,
    })
}

fn open_source(source_path: &Path) -> Result<Repository> {
    match Repository::discover(source_path) {
        Ok(repo) => Ok(repo),
        Err(_) => Repository::open(source_path),
    }
}

fn apply_copied_refs(
    target: &Repository,
    updates: &[(String, ObjectId)],
    source_path: &Path,
    options: &LocalCloneOptions,
) -> Result<()> {
    let refs = target.references();
    let committer = target.default_reflog_committer()?;
    let message = options.get_reflog_message().map_or_else(
        || format!("clone: copied from {}", source_path.display()),
        str::to_string,
    );
    let changes = updates
        .iter()
        .map(|(name, oid)| -> Result<RefChange> {
            // Record the destination's current tip as the reflog old side so a
            // re-copy reads as an update rather than a second birth.
            let old_oid = match refs.read_ref(name)? {
                Some(RefTarget::Direct(current)) => current,
                _ => ObjectId::null(target.object_format()),
            };
            Ok(RefChange {
                name: FullName::new(name)?,
                new: RefTarget::Direct(*oid),
                expected: None,
                reflog: Some(ReflogEntry {
                    old_oid,
                    new_oid: *oid,
                    committer: committer.clone(),
                    message: message.clone().into_bytes(),
                }),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    target
        .apply_ref_changes(&changes)
        .map_err(|conflict| GitError::Transaction(conflict.message))?;
    Ok(())
}

/// Point the destination's `HEAD` at the branch the source had checked out,
/// honouring it whenever it names a branch that was actually copied.
///
/// A clone must not silently move a user from `master`/`trunk` to `main` just
/// because a `main` branch also exists. Fall back to `main`, then to the
/// alphabetically-first copied branch, only when the source HEAD is detached
/// or points at a branch we did not import.
fn mirror_source_head(
    source: &Repository,
    target: &Repository,
    updates: &[(String, ObjectId)],
) -> Result<Option<String>> {
    let copied_branches: HashSet<&str> = updates
        .iter()
        .filter_map(|(name, _)| name.strip_prefix("refs/heads/"))
        .collect();
    if copied_branches.is_empty() {
        return Ok(None);
    }
    let source_branch = source
        .head()
        .ok()
        .and_then(|head| head.branch_name().map(str::to_owned))
        .filter(|branch| copied_branches.contains(branch.as_str()));
    let chosen = source_branch.or_else(|| {
        copied_branches
            .contains("main")
            .then(|| "main".to_string())
            .or_else(|| copied_branches.iter().min().map(|name| (*name).to_string()))
    });
    let Some(branch) = chosen else {
        return Ok(None);
    };
    target
        .set_head_symref(format!("refs/heads/{branch}"), HeadUpdateOptions::new())
        .map_err(|conflict| GitError::Transaction(conflict.message))?;
    Ok(Some(branch))
}
