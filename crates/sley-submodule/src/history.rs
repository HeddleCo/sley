//! Submodule commit-history presentation for `diff --submodule=log` output:
//! the `old..new`/`old...new` range marker, symmetric-difference commit walk,
//! and one-line subject rendering. Ported from the CLI's former
//! `diff_render.rs` inline-diff tier.
//!
//! These helpers only need revision walking plus object/config access, so —
//! unlike the worktree-bound dirt probes — they can live in this crate.

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

/// Renders a commit as the one-line log entry (`<marker> <subject>` body);
/// injected so this crate does not depend on the log-formatting crate.
pub type RenderSubject<'a> = &'a (dyn Fn(&Commit) -> String + 'a);

/// The commit a gitlink entry's new side resolves to: its recorded oid, or —
/// for a worktree comparison where changed-path oids are unresolved — the
/// submodule's live HEAD, falling back to the old side's oid.
pub fn new_gitlink_oid(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree: bool,
) -> Result<Option<ObjectId>> {
    if entry.new_mode != Some(0o160000) {
        return Ok(None);
    }
    Ok(match entry.new_oid {
        Some(oid) => Some(oid),
        None => match (use_worktree, worktree_root) {
            (true, Some(root)) => {
                let sub_root =
                    root.join(sley_diff_merge::porcelain::repo_path_to_path(&entry.path));
                sley_diff_merge::gitlink_head_oid(&sub_root, db.object_format()).or(entry.old_oid)
            }
            _ => entry.old_oid,
        },
    })
}

/// The tree of the submodule commit at `oid`; the empty tree for a null oid
/// (a newly added or deleted submodule side).
pub fn submodule_commit_tree(db: &FileObjectDatabase, oid: &ObjectId) -> Result<ObjectId> {
    if oid.is_null() {
        return Ok(ObjectId::empty_tree(db.object_format()));
    }
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!("expected commit {oid}")));
    }
    Ok(Commit::parse(db.object_format(), &object.body)?.tree)
}

/// The `..`/`...` range marker between two submodule commits: `..` when one
/// side reaches the other (fast-forward or rewind), `...` otherwise, plus
/// whether the range is a rewind (the new side is an ancestor of the old).
pub fn submodule_range_marker(
    git_dir: Option<&Path>,
    db: Option<&FileObjectDatabase>,
    format: Option<ObjectFormat>,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
) -> Result<(&'static str, bool)> {
    let (Some(git_dir), Some(db), Some(format)) = (git_dir, db, format) else {
        return Ok(("...", false));
    };
    if old_oid.is_null() || new_oid.is_null() {
        return Ok(("...", false));
    }
    let bases = sley_rev::merge_bases(git_dir, format, db, old_oid, new_oid)?;
    let fast_forward = bases.iter().any(|base| base == old_oid);
    let rewind = bases.iter().any(|base| base == new_oid);
    Ok((if fast_forward || rewind { ".." } else { "..." }, rewind))
}

/// Commits unique to either side of the `old`/`new` submodule tips, marked
/// `<` (only on the old side) or `>` (only on the new side) as upstream's
/// `submodule summary` walk does.
pub fn submodule_symmetric_records(
    db: &FileObjectDatabase,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
) -> Result<Vec<(char, sley_rev::CommitRecord)>> {
    if old_oid.is_null() || new_oid.is_null() {
        return Ok(Vec::new());
    }
    let left = sley_rev::walk_commits(db, db.object_format(), [*old_oid])?;
    let right = sley_rev::walk_commits(db, db.object_format(), [*new_oid])?;
    let left_set = left.iter().map(|record| record.oid).collect::<HashSet<_>>();
    let right_set = right
        .iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    let mut marked = Vec::new();
    marked.extend(
        right
            .into_iter()
            .filter(|record| !left_set.contains(&record.oid))
            .map(|record| ('>', record)),
    );
    marked.extend(
        left.into_iter()
            .filter(|record| !right_set.contains(&record.oid))
            .map(|record| ('<', record)),
    );
    Ok(marked)
}

/// Render the `git diff --submodule=log` commit list between two submodule
/// tips: one `<`/`>`-marked subject line per commit outside the merge bases.
pub fn write_submodule_log(
    stdout: &mut dyn Write,
    git_dir: Option<&Path>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    render_subject: RenderSubject<'_>,
) -> Result<()> {
    let Some(git_dir) = git_dir else {
        return Ok(());
    };
    let bases = if old_oid.is_null() || new_oid.is_null() {
        HashSet::new()
    } else {
        sley_rev::merge_bases(git_dir, format, db, old_oid, new_oid)?
            .into_iter()
            .collect()
    };
    for (marker, record) in submodule_symmetric_records(db, old_oid, new_oid)? {
        if bases.contains(&record.oid) {
            continue;
        }
        let subject = render_subject(&record.commit);
        writeln!(stdout, "  {marker} {subject}")?;
    }
    Ok(())
}
