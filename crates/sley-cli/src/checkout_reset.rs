//! Checkout/reset ref updates and worktree resolution.

use std::io::{self, Write};
use std::path::Path;
use std::path::PathBuf;

use sley::plumbing::sley_object::{Commit, ObjectType};
use sley::plumbing::sley_odb::{FileObjectDatabase, ObjectReader};
use sley::plumbing::sley_refs::{FileRefStore, RefUpdate, ReflogEntry, branch_ref_name};
use sley::{GitError, ObjectFormat, ObjectId, ReferenceTarget as RefTarget, Result};

use crate::commands::remote::read_repo_config;
use crate::commit_subject_bytes;
use crate::format_log_abbrev_oid;
use crate::log_output_encoding;
use crate::log_reencode_message;
use crate::resolve_revision;
use crate::setup;
use crate::sley_rev;
use crate::sley_worktree;

pub(crate) fn update_reset_head_ref(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    target: &str,
    committer: Vec<u8>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let reflog = |old_oid: ObjectId, new_oid: ObjectId| ReflogEntry {
        old_oid,
        new_oid,
        committer: committer.clone(),
        message: format!("reset: moving to {target}").into_bytes(),
    };
    let mut tx = store.transaction();
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => {
            tx.update(RefUpdate {
                name: name.clone(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog(old_oid, new_oid)),
            });
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(name),
                reflog: Some(reflog(old_oid, new_oid)),
            });
        }
        _ => {
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog(old_oid, new_oid)),
            });
        }
    }
    tx.commit()
}

pub(crate) fn print_reset_hard_head(
    git_dir: &Path,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            commit_oid,
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    // git's "HEAD is now at" line re-encodes the subject from the commit's stored
    // `encoding` header to the log output encoding (i18n.logOutputEncoding, else
    // i18n.commitEncoding, else UTF-8) — t7102 cells 7/8. Write the result as raw
    // bytes since a non-UTF-8 output encoding (e.g. ISO8859-1) is not valid UTF-8.
    let config = read_repo_config(git_dir)?;
    let from = commit
        .encoding
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    let to = log_output_encoding(&config);
    let reencoded = log_reencode_message(commit.message, &from, &to);
    let subject = commit_subject_bytes(&reencoded);
    let mut stdout = io::stdout().lock();
    write!(
        stdout,
        "HEAD is now at {} ",
        format_log_abbrev_oid(commit_oid)
    )?;
    stdout.write_all(&subject)?;
    writeln!(stdout)?;
    Ok(())
}

/// Git clean file selection without `-d` or pathspecs: a worktree-root file is
/// always eligible; a file in a subdirectory is eligible only when its immediate
/// parent directory contains tracked content (otherwise the file lives in a
/// wholly-untracked directory that Git would only remove under `-d`). This holds
pub(crate) fn checkout_create_or_reset_branch(
    git_dir: &Path,
    start_git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    branch: &str,
    start: &str,
    force: bool,
    create_reflog: bool,
    committer: Vec<u8>,
) -> Result<bool> {
    let store = FileRefStore::new(git_dir, format);
    if branch == "HEAD" || branch == "@" {
        eprintln!("fatal: '{branch}' is not a valid branch name");
        return Err(GitError::Exit(128));
    }
    let name = branch_ref_name(branch)?;
    let existing = store.read_ref(&name)?;
    if existing.is_some() && !force {
        eprintln!("fatal: a branch named '{branch}' already exists");
        return Err(GitError::Exit(128));
    }
    // The start point (often the implicit "HEAD") is resolved against the
    // worktree the command runs from — `git worktree add` from a linked
    // worktree branches off *that* worktree's HEAD.
    let start_oid = match resolve_checkout_start_oid(start_git_dir, format, start, replace_objects)
    {
        Ok(Some(start_oid)) => start_oid,
        Ok(None) => {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(name),
                reflog: None,
            });
            tx.commit()?;
            return Ok(false);
        }
        Err(err) => return Err(err),
    };
    let db = crate::repository::open_object_database(start_git_dir, format, replace_objects)?;
    let start_oid = sley_rev::peel_to_commit(&db, format, &start_oid)?;
    if let Some(existing) = existing {
        let old_oid = match existing {
            RefTarget::Direct(oid) => oid,
            RefTarget::Symbolic(_) => {
                return Err(GitError::Unsupported(
                    "checkout -B target branch must be direct".into(),
                ));
            }
        };
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(start_oid),
            reflog: Some(ReflogEntry {
                old_oid,
                new_oid: start_oid,
                committer,
                message: format!("branch: Reset to {start}").into_bytes(),
            }),
        });
        tx.commit()?;
        Ok(true)
    } else {
        let reflog = store
            .should_write_reflog_for_update(&name, create_reflog)?
            .then(|| ReflogEntry {
                old_oid: ObjectId::null(format),
                new_oid: start_oid,
                committer,
                message: format!("branch: Created from {start}").into_bytes(),
            });
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(start_oid),
            reflog,
        });
        tx.commit()?;
        Ok(false)
    }
}

pub(crate) fn resolve_checkout_start_oid(
    git_dir: &Path,
    format: ObjectFormat,
    start: &str,
    replace_objects: bool,
) -> Result<Option<ObjectId>> {
    if let Some(oid) =
        resolve_checkout_merge_base_start_oid(git_dir, format, start, replace_objects)?
    {
        return Ok(Some(oid));
    }
    match resolve_revision(git_dir, format, start, replace_objects) {
        Ok(oid) => Ok(Some(oid)),
        Err(_) if start == "HEAD" || start == "@" => {
            let store = FileRefStore::new(git_dir, format);
            match store.read_ref("HEAD")? {
                Some(RefTarget::Symbolic(name)) if store.read_ref(&name)?.is_none() => Ok(None),
                _ => Err(GitError::not_found(format!("revision {start}"))),
            }
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn resolve_checkout_merge_base_start_oid(
    git_dir: &Path,
    format: ObjectFormat,
    start: &str,
    replace_objects: bool,
) -> Result<Option<ObjectId>> {
    let Some((left, right)) = start.split_once("...") else {
        return Ok(None);
    };
    if right.contains("...") {
        return Ok(None);
    }
    let left = if left.is_empty() { "HEAD" } else { left };
    let right = if right.is_empty() { "HEAD" } else { right };
    let db = crate::repository::open_object_database(git_dir, format, replace_objects)?;
    let resolver = sley_rev::RevisionResolver::new(git_dir, format, &db);
    let left = sley_rev::peel_to_commit(&db, format, &resolver.resolve(left)?)?;
    let right = sley_rev::peel_to_commit(&db, format, &resolver.resolve(right)?)?;
    let bases = sley_rev::merge_bases(git_dir, format, &db, &left, &right)?;
    match bases.as_slice() {
        [base] => Ok(Some(*base)),
        [] => {
            eprintln!("fatal: no merge base found");
            Err(GitError::Exit(128))
        }
        _ => {
            eprintln!("fatal: multiple merge bases found");
            Err(GitError::Exit(128))
        }
    }
}

pub(crate) fn require_work_tree(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
) -> Result<PathBuf> {
    if let Some(result) = setup::setup_git_directory(cli_session) {
        if result.worktree_config_bogus {
            eprintln!("warning: core.bare and core.worktree do not make sense");
            eprintln!("fatal: unable to set up work tree using invalid config");
            return Err(GitError::Exit(128));
        }
        if let Some(worktree) = result.worktree {
            return Ok(worktree);
        }
        eprintln!("fatal: this operation must be run in a work tree");
        return Err(GitError::Exit(128));
    }
    match sley_worktree::worktree_root_for_git_dir(git_dir)? {
        Some(root) => Ok(root),
        None => {
            eprintln!("fatal: this operation must be run in a work tree");
            Err(GitError::Exit(128))
        }
    }
}
