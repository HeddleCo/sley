//! The rebase `--merge` drive loop (sequencer.c's todo machine).
//!
//! Stage-B relocation from the CLI: everything that executes the instruction
//! sheet now lives with the engine — the `complete_action` editor round, the
//! `pick_commits` loop and every todo verb (`pick`/`reword`/`edit`/`fixup`/
//! `squash`/`exec`/`break`/`label`/`reset`/`merge`/`update-ref`), commit
//! creation for the machine ([`machine_commit`]), the fixup/squash message
//! machinery, rewritten-commit tracking (`rewritten-list`/`rewritten-pending`),
//! the `--continue`/`--skip`/`--abort`/`--quit`/`--edit-todo` transitions,
//! update-refs bookkeeping, and autostash integration. The CLI keeps argv
//! parsing, usage text, progress rendering policy, exit codes, and the
//! host-injected services below.
//!
//! Byte-parity notes:
//! * Diagnostics that are part of git's porcelain contract are emitted here
//!   verbatim (same strings as the previous CLI home), matching the
//!   established library-crate pattern (see sley-worktree, [`super::pick`]).
//! * Partial-clone hydration is injected via
//!   [`crate::apply::PromisorObjectFetch`] like every blob read in the apply
//!   backend.
//! * Notes copying stays behind a host seam: `sley-notes` depends on this
//!   crate, so the note-copy primitive cannot live here without a cycle. The
//!   rewritten-pair policy (flush cadence, list parsing, warning emission) is
//!   owned by [`run_post_rewrite_hook`].
//!
//! This module is a faithful move; the module-level lint allowances mirror the
//! source file's (`commands/rebase.rs`) so error paths keep their exact
//! control flow.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::pick::{comment_char, strip_comment_lines, strip_comment_string_lines};
use super::rebase as sheet;
use super::rebase::{
    LoadTodoListOutcome, RebaseState, RebaseTodoItem, RebaseTodoList, TodoCommand, TodoOidLookup,
    TodoRenderOptions,
};
use crate::apply::{
    commit_tree_oid, head_commit_oid, merge_favor_from_strategy_opts, merge_index_entry,
    merge_read_blob_with_fetch, merge_remove_worktree_file, merge_rename_limit_from_config,
    merge_write_worktree_file, merge_ws_ignore_from_strategy_opts, directory_renames_from_config,
    three_way_merge_trees_inner_with_info_opts, three_way_merge_trees_inner_with_info_opts_and_path_favor,
    MergePathResult, MergePathResults, PromisorObjectFetch, RenameMergeConfig,
};
use sley_config::{effective_config_parameters_env, GitConfig};
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_diff_merge::{flatten_tree, ConflictStyle, WsIgnore};
use sley_index::{Index, IndexEntry};
use sley_object::{
    canonicalize_commit_date, commit_identity_from_env, commit_identity_from_env_with_date,
    commit_signoff_from_env, committer_identity_for_reflog,
};
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_pretty::{
    commit_author_for_commit_encoding, commit_encoding_config, commit_encoding_header_from_config,
    commit_message_for_commit_encoding, commit_subject,
};
use sley_refs::{
    resolve_ref_peeled, FileRefStore, RefPrecondition, RefTarget, RefUpdate, ReflogEntry,
};
use sley_rev::revlist::read_rev_list_commit_record;
use sley_rev::{is_ancestor, merge_bases, peel_to_commit, resolve_revision_with_replacement_policy};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub type MachineOpts = RebaseState;
pub type TodoList = RebaseTodoList;

// ---------------------------------------------------------------------------
// Context + host services
// ---------------------------------------------------------------------------

/// Repository handles shared by every rebase merge-backend operation. The CLI
/// builds this from its open [`sley::Repository`] (git dir layout, config
/// cascade, ref stores) and hands it to the engine; nothing in here reaches
/// back into process/session state.
pub struct RebaseContext {
    pub git_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub worktree_root: PathBuf,
    pub format: ObjectFormat,
    pub config: GitConfig,
    /// Per-worktree ref store (HEAD, checked-out branch, `refs/rewritten/*`).
    pub refs: FileRefStore,
    /// Common-gitdir ref store (shared branches, `update-refs` targets).
    pub common_refs: FileRefStore,
    db: FileObjectDatabase,
    /// `GIT_REFLOG_ACTION` or `"rebase"`.
    pub reflog_action: String,
    pub replace_objects: bool,
    pub recurse_submodules: bool,
}

impl RebaseContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        git_dir: PathBuf,
        common_git_dir: PathBuf,
        worktree_root: PathBuf,
        format: ObjectFormat,
        config: GitConfig,
        refs: FileRefStore,
        common_refs: FileRefStore,
        db: FileObjectDatabase,
        reflog_action: String,
        replace_objects: bool,
        recurse_submodules: bool,
    ) -> Self {
        RebaseContext {
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            config,
            refs,
            common_refs,
            db,
            reflog_action,
            replace_objects,
            recurse_submodules,
        }
    }

    pub fn db(&self) -> FileObjectDatabase {
        self.db.clone()
    }

    pub fn refs(&self) -> &FileRefStore {
        &self.refs
    }

    pub fn state_path(&self, name: &str) -> PathBuf {
        sheet::state_path(&self.git_dir, name)
    }

    pub fn reflog(&self, sub_action: &str, rest: Option<&str>) -> Vec<u8> {
        let mut out = format!("{} ({sub_action})", self.reflog_action);
        if let Some(rest) = rest {
            out.push_str(": ");
            out.push_str(rest);
        }
        out.into_bytes()
    }
}

/// Host services the drive loop cannot own: process-spawning hooks/editor,
/// status collection over the session view, abbreviation policy, diff/summary
/// renderers, stash primitives, and rerere wiring (whose resolved-stage hook
/// stages index entries from the porcelain side).
///
/// Every field mirrors one call site group of the previous CLI implementation;
/// see the corresponding function docs for the exact contract.
#[allow(clippy::type_complexity)]
pub struct RebaseHosts<'a> {
    /// Partial-clone hydration for blob reads; `None` outside partial clones.
    pub promisor_fetch: Option<&'a dyn PromisorObjectFetch>,
    /// `git status --porcelain`-equivalent entry collection over the session's
    /// worktree/git-dir view (pathspec filters, fsync, sparse rules).
    pub short_status: Box<dyn Fn() -> Result<Vec<sley_worktree::ShortStatusEntry>> + 'a>,
    /// Effective `core.abbrev` width policy (global config + auto sizing).
    /// Returns the full hex length when abbreviations are disabled.
    pub abbrev_width: Box<dyn Fn() -> usize + 'a>,
    /// Reset index+worktree to a commit with submodule recursion
    /// (`rebase.recurseSubmodules`). Only called when `recurse_submodules`.
    pub reset_submodules: Box<dyn Fn(&ObjectId) -> Result<()> + 'a>,
    /// Launch `$GIT_SEQUENCE_EDITOR`/`sequence.editor`/… on the todo file.
    pub launch_sequence_editor: Box<dyn Fn(&Path) -> Result<()> + 'a>,
    /// `render_commit_editor_status_for_rebase`: the commented status block
    /// appended to COMMIT_EDITMSG templates during machine commits.
    pub editor_status_block: Box<dyn Fn(&str, bool /* amend */) -> Result<Vec<u8>> + 'a>,
    /// Finalize the commit message: write `.git/COMMIT_EDITMSG` from the seed
    /// bytes, run the prepare-commit-msg hook (source: HEAD amend when
    /// `commit_head`, CHERRY_PICK_HEAD merge when `merge`, else `message`),
    /// launch the editor + commit-msg hook when `edit`, and return the raw
    /// (uncleaned) file contents.
    pub prepare_commit_message:
        Box<dyn Fn(&Path, Vec<u8>, bool /* commit_head */, bool /* merge */, bool /* edit */) -> Result<Vec<u8>> + 'a>,
    /// Run a lifecycle hook (`post-checkout`, `post-rewrite`, `post-commit`)
    /// with args and optional stdin payload.
    pub run_hook: Box<dyn Fn(&str, Vec<String>, Option<Vec<u8>>) -> Result<()> + 'a>,
    /// `render_tree_to_tree_patch`: the stop-state patch (`rebase-merge/patch`,
    /// REBASE_HEAD display) as bytes.
    pub tree_patch: Box<dyn Fn(&ObjectId, &ObjectId) -> Result<Vec<u8>> + 'a>,
    /// `--continue` commit summary (`branch summary` + shortstat between
    /// trees); only invoked when the continue commit is not quiet.
    pub print_continue_summary: Box<dyn Fn(&ObjectId, &[u8], ObjectId, ObjectId) -> Result<()> + 'a>,
    /// Finish diffstat (`orig-head..HEAD`); only invoked when verbose.
    pub print_diffstat: Box<dyn Fn(&ObjectId, &ObjectId) -> Result<()> + 'a>,
    /// `git stash create autostash`: stash-commit oid without touching
    /// `refs/stash`; `None` when the tree is clean.
    pub stash_create: Box<dyn Fn() -> Result<Option<ObjectId>> + 'a>,
    /// Apply a stash commit quietly; `Ok(false)` when it cannot apply cleanly.
    pub stash_apply_quietly: Box<dyn Fn(&ObjectId) -> Result<bool> + 'a>,
    /// `git stash store <oid> <message>`.
    pub stash_store: Box<dyn Fn(&ObjectId, &str) -> Result<()> + 'a>,
    /// Record conflicts in the rerere database and replay known resolutions
    /// (staging them under `rerere.autoupdate`). Mirrors the CLI wrapper's
    /// resolved-stage hook wiring.
    pub rerere_now: Box<dyn Fn(Option<bool>) -> Result<bool> + 'a>,
    /// Record the just-committed resolution after a conflict is committed.
    pub rerere_record_resolved: Box<dyn Fn() -> Result<()> + 'a>,
    /// Append a Signed-off-by trailer ahead of any trailing comment block.
    pub append_signoff: Box<dyn Fn(Vec<u8>, &[u8]) -> Vec<u8> + 'a>,
    /// Sign a commit payload (key resolution + gpg/agent access are
    /// host-owned). Decides `commit.gpgsign`/`-S` policy itself and returns
    /// `None` when the rebase is not signing.
    #[allow(clippy::type_complexity)]
    pub commit_signature: Box<
        dyn Fn(
                ObjectId,
                &[ObjectId],
                &[u8],
                &[u8],
                &[u8],
                Option<Vec<u8>>,
                &RebaseState,
            ) -> Result<Option<Vec<u8>>>
            + 'a,
    >,
    /// Copy notes from rewritten commits to their replacements per
    /// `notes.rewrite.*` (host-owned because sley-notes sits above this crate).
    pub copy_notes_for_rewrite: Box<dyn Fn(&[(ObjectId, ObjectId)]) -> Result<()> + 'a>,
}

fn config_value(config: &GitConfig, section: &str, key: &str) -> Option<String> {
    // A linked worktree's administrative gitdir contains HEAD/index/rebase
    // state, while repository configuration remains in the common gitdir.
    // Reading `<worktrees/name>/config` silently loses settings such as
    // sequence.editor, causing an interactive rebase in a linked worktree to
    // ignore its configured todo editor entirely.
    config.get(section, None, key).map(str::to_string)
}

fn config_bool(config: &GitConfig, section: &str, key: &str) -> Option<bool> {
    let value = config_value(config, section, key)?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

fn rebase_config_value(ctx: &RebaseContext, section: &str, key: &str) -> Option<String> {
    config_value(&ctx.config, section, key)
}

fn rebase_config_bool(ctx: &RebaseContext, section: &str, key: &str) -> Option<bool> {
    config_bool(&ctx.config, section, key)
}

/// The effective `core.commentChar` string (git's `comment_line_str`), default
/// `#`. May be multi-char; `auto` resolves provisionally to `#` (the rebase
/// machine never runs the commit-time unused-char scan).
fn commit_comment_string(git_dir: &Path) -> String {
    let value = sley_config::read_repo_config(git_dir, effective_config_parameters_env().as_deref())
        .ok()
        .and_then(|c| {
            c.get("core", None, "commentchar")
                .or_else(|| c.get("core", None, "commentstring"))
                .map(str::to_string)
        });
    match value.as_deref() {
        None | Some("") => "#".to_string(),
        Some(v) if v.eq_ignore_ascii_case("auto") => "#".to_string(),
        Some(v) => v.to_string(),
    }
}

fn warn_comment_char_auto(ctx: &RebaseContext) {
    if !rebase_config_value(ctx, "core", "commentChar")
        .is_some_and(|value| value.eq_ignore_ascii_case("auto"))
    {
        return;
    }
    eprintln!(
        "warning: Support for 'core.commentChar=auto' is deprecated and will be removed in Git 3.0"
    );
    eprintln!("hint: ");
    eprintln!("hint: To use the default comment string (#) please run");
    eprintln!("hint: ");
    eprintln!("hint:     git config unset core.commentChar");
    eprintln!("hint: ");
    eprintln!("hint: To set a custom comment string please run");
    eprintln!("hint: ");
    eprintln!("hint:     git config set core.commentChar <comment string>");
    eprintln!("hint: ");
    eprintln!("hint: where '<comment string>' is the string you wish to use.");
}

#[derive(PartialEq, Eq)]
enum MissingCommitCheck {
    Ignore,
    Warn,
    Error,
}

fn missing_commit_check_level(ctx: &RebaseContext) -> MissingCommitCheck {
    match rebase_config_value(ctx, "rebase", "missingCommitsCheck").as_deref() {
        Some(value) if value.eq_ignore_ascii_case("warn") => MissingCommitCheck::Warn,
        Some(value) if value.eq_ignore_ascii_case("error") => MissingCommitCheck::Error,
        _ => MissingCommitCheck::Ignore,
    }
}

/// git's rebase clean-check / autostash dirty-detection skips submodule
/// gitlinks (has_unstaged_changes runs with `ignore_submodules = 1`; t3426).
pub fn rebase_status_is_submodule(entry: &sley_worktree::ShortStatusEntry) -> bool {
    entry.submodule.is_some()
        || [entry.head_mode, entry.index_mode, entry.worktree_mode]
            .into_iter()
            .flatten()
            .any(sley_index::is_gitlink)
}

fn is_submodule_only_status(entry: &sley_worktree::ShortStatusEntry) -> bool {
    entry.submodule.is_some()
        && entry.index == b' '
        && entry.worktree == b'M'
        && entry.index_mode.is_some_and(sley_index::is_gitlink)
}

// ---------------------------------------------------------------------------
// Plumbing: reset/checkout/detach + abbreviation
// ---------------------------------------------------------------------------

pub fn reset_index_and_worktree_to_commit_for_rebase(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    commit: &ObjectId,
) -> Result<()> {
    if ctx.recurse_submodules {
        return (hosts.reset_submodules)(commit);
    }
    sley_worktree::reset_index_and_worktree_to_commit_with_process_filter_metadata(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        commit,
        rebase_process_filter_metadata(ctx, commit),
    )?;
    Ok(())
}

fn rebase_process_filter_metadata(
    ctx: &RebaseContext,
    commit: &ObjectId,
) -> Option<sley_worktree::ProcessFilterMetadata> {
    let mut metadata = Vec::new();
    let head_name = sheet::read_state_line(&ctx.git_dir, "head-name")
        .map(|name| name.trim().to_string())
        .or_else(|| ctx.refs().current_branch_ref().ok().flatten());
    if let Some(head_name) = head_name
        && head_name.starts_with("refs/")
    {
        metadata.push(("ref".to_string(), head_name));
    }
    metadata.push(("treeish".to_string(), commit.to_hex()));
    Some(metadata)
}

fn make_resolver<'a>(
    ctx: &'a RebaseContext,
    db: &'a FileObjectDatabase,
) -> impl FnMut(&str) -> TodoOidLookup + 'a {
    move |token: &str| {
        let Ok(oid) =
            resolve_revision_with_replacement_policy(&ctx.git_dir, ctx.format, token, ctx.replace_objects)
        else {
            return TodoOidLookup::Missing;
        };
        let Ok(peeled) = peel_to_commit(db, ctx.format, &oid) else {
            return TodoOidLookup::Missing;
        };
        let Ok(record) = read_rev_list_commit_record(db, ctx.format, peeled) else {
            return TodoOidLookup::Missing;
        };
        TodoOidLookup::Commit {
            oid: record.oid,
            parents: record.parents.len(),
        }
    }
}

/// Shortest unambiguous hex under the repository's `core.abbrev` policy.
fn find_unique_abbrev_hex(
    hosts: &RebaseHosts<'_>,
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> String {
    let hex = oid.to_hex();
    sheet::unique_abbrev(db, oid, (hosts.abbrev_width)().min(hex.len()))
}

/// Crash-safe publication for operation-state files (`git-rebase-todo`,
/// `done`, `update-refs`, message state, `MERGE_MSG`, ...): write to a
/// sibling `.lock` temp file and rename over the final path. The rename is
/// atomic within the filesystem, so a crash mid-write can never leave a
/// truncated or partial file at the final path — THE crash-recovery surface
/// for `--continue`/`--abort`. Bytes-on-disk semantics are unchanged: same
/// content at the same final path.
fn write_state_atomic(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()> {
    sley_core::atomic::atomic_write(path.as_ref(), contents.as_ref())
}

fn todo_render_options(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    short: bool,
    abbreviate: bool,
) -> TodoRenderOptions {
    let minimum_abbrev = short.then(|| {
        if abbreviate {
            7
        } else {
            (hosts.abbrev_width)().min(ctx.format.hex_len())
        }
    });
    TodoRenderOptions {
        minimum_abbrev,
        abbreviate_commands: abbreviate,
    }
}

#[allow(clippy::too_many_arguments)]
fn write_todo_file(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    path: &Path,
    items: &[RebaseTodoItem],
    short: bool,
    help: bool,
    shortrevisions: Option<&str>,
    shortonto: Option<&str>,
    db: &FileObjectDatabase,
) -> Result<()> {
    let abbreviate = help && rebase_config_bool(ctx, "rebase", "abbreviateCommands").unwrap_or(false);
    let mut buf = sheet::render_todo_list(db, items, todo_render_options(ctx, hosts, short, abbreviate));
    if help {
        let comment = comment_char(&ctx.git_dir) as char;
        let check_error = missing_commit_check_level(ctx) == MissingCommitCheck::Error;
        sheet::append_todo_help(
            &mut buf,
            sheet::count_commands(items),
            shortrevisions,
            shortonto,
            comment,
            check_error,
        );
    }
    write_state_atomic(path, buf)?;
    Ok(())
}

/// `save_todo`: persist the not-yet-executed tail, append the current item to
/// `done`.
fn save_todo(
    ctx: &RebaseContext,
    todo: &TodoList,
    db: &FileObjectDatabase,
    reschedule: bool,
) -> Result<()> {
    sheet::save_rebase_todo_list(&ctx.git_dir, db, todo, reschedule)
}

fn read_populate_todo(ctx: &RebaseContext, db: &FileObjectDatabase) -> Result<TodoList> {
    let mut resolver = make_resolver(ctx, db);
    match sheet::load_rebase_todo_list(&ctx.git_dir, comment_char(&ctx.git_dir) as char, &mut resolver)? {
        LoadTodoListOutcome::Ready(todo) => Ok(todo),
        LoadTodoListOutcome::Invalid { messages } => {
            for message in messages {
                eprintln!("{message}");
            }
            eprintln!("error: please fix this using 'git rebase --edit-todo'.");
            Err(GitError::Exit(1))
        }
    }
}

// ---------------------------------------------------------------------------
// update-refs runtime state
// ---------------------------------------------------------------------------

/// One `(refname, before, after)` record in the `update-refs` state file. The
/// file stores three lines per ref; `after` is the all-zero OID until the ref's
/// `update-ref` todo command runs (recording the then-current HEAD).
struct UpdateRefRecord {
    refname: String,
    before: ObjectId,
    after: ObjectId,
}

fn read_update_refs_state(ctx: &RebaseContext) -> Result<Vec<UpdateRefRecord>> {
    let path = ctx.state_path("update-refs");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let mut lines = text.lines();
    let mut out = Vec::new();
    while let Some(refname) = lines.next() {
        let (Some(before), Some(after)) = (lines.next(), lines.next()) else {
            break;
        };
        let before = ObjectId::from_hex(ctx.format, before.trim())
            .map_err(|_| GitError::InvalidObject("invalid update-refs before-oid".into()))?;
        let after = ObjectId::from_hex(ctx.format, after.trim())
            .map_err(|_| GitError::InvalidObject("invalid update-refs after-oid".into()))?;
        out.push(UpdateRefRecord {
            refname: refname.to_string(),
            before,
            after,
        });
    }
    Ok(out)
}

fn write_update_refs_records(ctx: &RebaseContext, records: &[UpdateRefRecord]) -> Result<()> {
    let path = ctx.state_path("update-refs");
    if records.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    let mut text = String::new();
    for rec in records {
        text.push_str(&rec.refname);
        text.push('\n');
        text.push_str(&rec.before.to_hex());
        text.push('\n');
        text.push_str(&rec.after.to_hex());
        text.push('\n');
    }
    write_state_atomic(&path, text)?;
    Ok(())
}

/// Port of upstream `do_update_ref`: record the current HEAD as the `after`
/// value for `refname` in the update-refs state (applied later at finish).
fn do_update_ref(ctx: &RebaseContext, refname: &str) -> Result<()> {
    let mut records = read_update_refs_state(ctx)?;
    if records.is_empty() {
        return Ok(());
    }
    let refs = ctx.refs();
    let head = head_commit_oid(refs)?.unwrap_or(ObjectId::null(ctx.format));
    for rec in &mut records {
        if rec.refname == refname {
            rec.after = head;
            break;
        }
    }
    write_update_refs_records(ctx, &records)
}

/// Port of upstream `do_update_refs`: at finish, apply every recorded ref
/// update (compare-and-swap `before` -> `after`), reporting the refs updated
/// and any that failed (e.g. moved out from under the rebase). Refs are
/// reported sorted by name. Returns an error if any update failed.
fn do_update_refs(ctx: &RebaseContext, quiet: bool) -> Result<()> {
    let mut records = read_update_refs_state(ctx)?;
    if records.is_empty() {
        return Ok(());
    }
    records.sort_by(|a, b| a.refname.cmp(&b.refname));
    let refs = &ctx.common_refs;
    let committer = committer_identity_for_reflog(&ctx.config)?;
    let zero = ObjectId::null(ctx.format);
    let mut updated = Vec::new();
    let mut failed = Vec::new();
    for rec in &records {
        // Skip refs whose update-ref command never ran (after still zero).
        if rec.after == zero {
            continue;
        }
        // Report and continue per-ref (like the CAS-failure arm below), not
        // abort-the-loop: one unresolvable ref must not prevent the others
        // from being updated.
        let current = match resolve_ref_peeled(refs, &rec.refname) {
            Ok(oid) => oid.unwrap_or(zero),
            Err(_) => {
                eprintln!("error: update_ref failed for ref '{}': ", rec.refname);
                failed.push(rec.refname.clone());
                continue;
            }
        };
        if current != rec.before {
            eprintln!("error: update_ref failed for ref '{}': ", rec.refname);
            failed.push(rec.refname.clone());
            continue;
        }
        let precondition = if rec.before == zero {
            RefPrecondition::ExistingMustMatch(RefTarget::Direct(zero))
        } else {
            RefPrecondition::MustExistAndMatch(RefTarget::Direct(rec.before))
        };
        let mut tx = refs.transaction();
        tx.update_to(
            rec.refname.clone(),
            RefTarget::Direct(rec.after),
            precondition,
            Some(ReflogEntry {
                old_oid: rec.before,
                new_oid: rec.after,
                committer: committer.clone(),
                message: b"rewritten during rebase".to_vec(),
            }),
        );
        match tx.commit() {
            Ok(()) => updated.push(rec.refname.clone()),
            Err(_) => {
                eprintln!("error: update_ref failed for ref '{}': ", rec.refname);
                failed.push(rec.refname.clone());
            }
        }
    }
    if !quiet && (!updated.is_empty() || !failed.is_empty()) {
        eprintln!("Updated the following refs with --update-refs:");
        for refname in &updated {
            eprintln!("\t{refname}");
        }
        if !failed.is_empty() {
            eprintln!("Failed to update the following refs with --update-refs:");
            for refname in &failed {
                eprintln!("\t{refname}");
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

/// Port of upstream `todo_list_filter_update_refs`: after the todo is (re)read
/// on `--continue`/`--edit-todo`, drop state entries whose ref no longer has an
/// `update-ref` line (and was not yet updated), and add un-updated entries for
/// any new `update-ref` lines.
fn filter_update_refs(ctx: &RebaseContext, items: &[RebaseTodoItem]) -> Result<()> {
    let mut records = read_update_refs_state(ctx)?;
    let zero = ObjectId::null(ctx.format);
    let todo_refs: HashSet<&str> = items
        .iter()
        .filter(|item| item.command == TodoCommand::UpdateRef)
        .map(|item| todo_arg_before_comment(&item.arg).trim())
        .collect();
    let mut updated = false;
    let before_len = records.len();
    records.retain(|rec| rec.after != zero || todo_refs.contains(rec.refname.as_str()));
    if records.len() != before_len {
        updated = true;
    }
    let store = &ctx.common_refs;
    for refname in &todo_refs {
        if !records.iter().any(|rec| rec.refname == *refname) {
            let before = resolve_ref_peeled(store, refname)?.unwrap_or(zero);
            records.push(UpdateRefRecord {
                refname: (*refname).to_string(),
                before,
                after: zero,
            });
            updated = true;
        }
    }
    if updated {
        write_update_refs_records(ctx, &records)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Checkout onto / detach helpers
// ---------------------------------------------------------------------------

/// Detach HEAD to `new_oid`, writing exactly one HEAD reflog entry.
pub(crate) fn detach_head_with_reflog(
    ctx: &RebaseContext,
    old_oid: ObjectId,
    new_oid: ObjectId,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    let refs = ctx.refs();
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: reflog_message,
        }),
    });
    tx.commit()
}

/// git's `reset_head`/`unpack_trees` aborts the detach-to-onto when checking out
/// the target tree would clobber an untracked working-tree file (a path present
/// in the onto tree whose worktree file is not tracked in the index and whose
/// content differs). Mirror that precondition: the blind
/// `reset_index_and_worktree_to_commit` would otherwise overwrite the file and
/// leave the rebase half-started (t3404 "abort with error when new base cannot be
/// checked out"). Returns the offending paths (empty ⇒ safe to proceed).
pub fn checkout_would_overwrite_untracked(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    target_tree: &ObjectId,
) -> Result<Vec<Vec<u8>>> {
    let target = flatten_tree(db, format, target_tree)?;
    let tracked: BTreeSet<Vec<u8>> =
        match sley_worktree::read_repository_index(git_dir, format)? {
            Some(index) => index
                .entries
                .iter()
                .filter(|entry| entry.stage() == sley_index::Stage::Normal)
                .map(|entry| entry.path.clone().into_bytes())
                .collect(),
            None => BTreeSet::new(),
        };
    let mut overwritten = Vec::new();
    for (path, (mode, oid)) in &target {
        if tracked.contains(path) {
            continue;
        }
        // Gitlinks are not materialized as ordinary files; skip them.
        if *mode == 0o160000 {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(path) else {
            continue;
        };
        let worktree_path = worktree_root.join(rel);
        let Ok(bytes) = fs::read(&worktree_path) else {
            continue;
        };
        let on_disk = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
        if on_disk != *oid {
            overwritten.push(path.clone());
        }
    }
    overwritten.sort();
    Ok(overwritten)
}

/// Context-bound convenience wrapper for the drive loop's call sites.
fn would_overwrite_untracked(
    ctx: &RebaseContext,
    db: &FileObjectDatabase,
    target_tree: &ObjectId,
) -> Result<Vec<Vec<u8>>> {
    checkout_would_overwrite_untracked(
        &ctx.git_dir,
        &ctx.worktree_root,
        ctx.format,
        db,
        target_tree,
    )
}

fn print_merge_would_overwrite_untracked(paths: &[Vec<u8>]) {
    eprintln!("error: The following untracked working tree files would be overwritten by merge:");
    for path in paths {
        eprintln!("\t{}", String::from_utf8_lossy(path));
    }
    eprintln!("Please move or remove them before you merge.");
    eprintln!("Aborting");
}

fn checkout_onto(ctx: &RebaseContext, hosts: &RebaseHosts<'_>, opts: &MachineOpts, onto_name: &str) -> Result<()> {
    checkout_onto_base(ctx, hosts, opts, onto_name, &opts.onto)
}

fn checkout_onto_base(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    onto_name: &str,
    base: &ObjectId,
) -> Result<()> {
    let refs = ctx.refs();
    let old = head_commit_oid(refs)?.unwrap_or(ObjectId::null(ctx.format));
    let db = ctx.db();
    let base_tree = commit_tree_oid(&db, ctx.format, base)?;
    let overwritten = would_overwrite_untracked(ctx, &db, &base_tree)?;
    if !overwritten.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by checkout:"
        );
        for path in &overwritten {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please move or remove them before you switch branches.");
        eprintln!("Aborting");
        apply_autostash(ctx, hosts);
        sheet::remove_merge_state(&ctx.git_dir);
        eprintln!("error: could not detach HEAD");
        return Err(GitError::Exit(1));
    }
    if let Err(err) = reset_index_and_worktree_to_commit_for_rebase(ctx, hosts, base) {
        apply_autostash(ctx, hosts);
        sheet::remove_merge_state(&ctx.git_dir);
        eprintln!("error: could not detach HEAD");
        let _ = err;
        return Err(GitError::Exit(1));
    }
    let committer = committer_identity_for_reflog(&ctx.config)?;
    detach_head_with_reflog(
        ctx,
        old,
        *base,
        ctx.reflog("start", Some(&format!("checkout {onto_name}"))),
        committer,
    )?;
    write_state_atomic(ctx.git_dir.join("ORIG_HEAD"), format!("{}\n", opts.orig_head))?;
    (hosts.run_hook)(
        "post-checkout",
        vec![old.to_hex(), base.to_hex(), "1".to_string()],
        None,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// complete_action: editor round + checkout onto + drive
// ---------------------------------------------------------------------------

/// `git rebase` start tail: write the todo + backup sheets, round-trip the
/// sequence editor when interactive, fast-forward leading picks that are
/// already on the base (`skip_unnecessary_picks`), and hand control to the
/// drive loop.
pub fn complete_action(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: MachineOpts,
    items: Vec<RebaseTodoItem>,
    upstream: Option<&ObjectId>,
    onto_name: &str,
    interactive: bool,
) -> Result<()> {
    let todo_path = ctx.state_path("git-rebase-todo");
    let backup_path = ctx.state_path("git-rebase-todo.backup");

    let shortonto = find_unique_abbrev_hex(hosts, &ctx.db(), &opts.onto);
    let shorthead = find_unique_abbrev_hex(hosts, &ctx.db(), &opts.orig_head);
    let shortrevisions = match upstream {
        Some(upstream) => {
            let shortrev = find_unique_abbrev_hex(hosts, &ctx.db(), upstream);
            format!("{shortrev}..{shorthead}")
        }
        None => shorthead,
    };

    warn_comment_char_auto(ctx);

    write_todo_file(
        ctx,
        hosts,
        &todo_path,
        &items,
        true,
        true,
        Some(&shortrevisions),
        Some(&shortonto),
        &ctx.db(),
    )?;
    write_todo_file(
        ctx,
        hosts,
        &backup_path,
        &items,
        false,
        true,
        Some(&shortrevisions),
        Some(&shortonto),
        &ctx.db(),
    )?;

    let mut new_items = items;
    if interactive {
        if let Err(err) = (hosts.launch_sequence_editor)(&todo_path) {
            apply_autostash(ctx, hosts);
            sheet::remove_merge_state(&ctx.git_dir);
            return Err(err);
        }
        let edited = fs::read_to_string(&todo_path)?;
        let stripped = stripspace_drop_comments(&edited, comment_char(&ctx.git_dir));
        if stripped.trim().is_empty() {
            apply_autostash(ctx, hosts);
            sheet::remove_merge_state(&ctx.git_dir);
            eprintln!("error: nothing to do");
            return Err(GitError::Exit(1));
        }
        let db = ctx.db();
        let mut resolver = make_resolver(ctx, &db);
        let (parsed, messages) = sheet::parse_todo_buffer(
            &stripped,
            false,
            comment_char(&ctx.git_dir) as char,
            &mut resolver,
        );
        if !messages.is_empty() {
            for message in messages {
                eprintln!("{message}");
            }
            print_edit_todo_recovery_advice();
            checkout_onto(ctx, hosts, &opts, onto_name)?;
            return Err(GitError::Exit(1));
        }
        // Missing-commit check against the original list.
        if check_todo_dropped_commits(ctx, hosts, &new_items, &parsed)? {
            checkout_onto(ctx, hosts, &opts, onto_name)?;
            return Err(GitError::Exit(1));
        }
        new_items = parsed;
    }

    // Reconcile the update-refs state with the (possibly user-edited) todo:
    // drop refs whose update-ref line was removed, add any newly-inserted ones.
    filter_update_refs(ctx, &new_items)?;

    let mut todo = TodoList {
        items: new_items,
        current: 0,
        done_nr: 0,
        total_nr: 0,
    };

    // skip_unnecessary_picks: leading picks already on the base fast-forward
    // into `done`.
    let mut base = opts.onto;
    if opts.allow_ff {
        let db = ctx.db();
        let mut skipped = 0usize;
        for item in &todo.items {
            if item.command == TodoCommand::Comment {
                break;
            }
            if item.command != TodoCommand::Pick {
                break;
            }
            let Some(oid) = &item.oid else { break };
            let record = read_rev_list_commit_record(&db, ctx.format, *oid)?;
            if record.parents.len() != 1 || record.parents[0] != base {
                break;
            }
            base = *oid;
            skipped += 1;
        }
        if skipped > 0 {
            let done_text = sheet::render_todo_list(
                &db,
                &todo.items[..skipped],
                todo_render_options(ctx, hosts, false, false),
            );
            write_state_atomic(ctx.state_path("done"), done_text)?;
            if todo
                .items
                .get(skipped)
                .is_some_and(|item| item.command.is_fixup())
            {
                for item in &todo.items[..skipped] {
                    if let Some(oid) = item.oid {
                        record_rewritten(ctx, &oid, Some(TodoCommand::Fixup))?;
                    }
                }
            }
            todo.items.drain(..skipped);
            todo.done_nr = skipped;
        }
    }

    let already_on_rebased_head = head_commit_oid(ctx.refs())? == Some(opts.orig_head);
    if todo.items.is_empty() && already_on_rebased_head && base == opts.orig_head {
        write_state_atomic(ctx.state_path("git-rebase-todo"), b"")?;
        write_state_atomic(ctx.state_path("end"), format!("{}\n", todo.done_nr))?;
        return finish_rebase(ctx, hosts, &opts);
    }

    write_todo_file(ctx, hosts, &todo_path, &todo.items, false, false, None, None, &ctx.db())?;
    todo.total_nr = todo.done_nr + sheet::count_commands(&todo.items);
    write_state_atomic(ctx.state_path("end"), format!("{}\n", todo.total_nr))?;

    checkout_onto_base(ctx, hosts, &opts, onto_name, &base)?;

    pick_commits(ctx, hosts, &opts, &mut todo)
}

fn stripspace_drop_comments(text: &str, comment: u8) -> String {
    let mut out = String::new();
    let mut blank_pending = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.as_bytes().first() == Some(&comment) {
            continue;
        }
        if trimmed.is_empty() {
            blank_pending = !out.is_empty();
            continue;
        }
        if blank_pending {
            out.push('\n');
            blank_pending = false;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out
}

fn check_todo_dropped_commits(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    old_items: &[RebaseTodoItem],
    new_items: &[RebaseTodoItem],
) -> Result<bool> {
    let level = missing_commit_check_level(ctx);
    if level == MissingCommitCheck::Ignore {
        return Ok(false);
    }
    let seen: HashSet<ObjectId> = new_items.iter().filter_map(|item| item.oid).collect();
    let mut missing = Vec::new();
    for item in old_items.iter().rev() {
        if item.command == TodoCommand::Drop {
            continue;
        }
        if let Some(oid) = &item.oid
            && !seen.contains(oid)
        {
            missing.push(format!(
                " - {} {}",
                find_unique_abbrev_hex(hosts, &ctx.db(), oid),
                item.arg
            ));
        }
    }
    if missing.is_empty() {
        return Ok(false);
    }
    eprintln!("Warning: some commits may have been dropped accidentally.");
    eprintln!("Dropped commits (newer to older):");
    for line in &missing {
        eprintln!("{line}");
    }
    eprintln!("To avoid this message, use \"drop\" to explicitly remove a commit.");
    eprintln!();
    eprintln!("Use 'git config rebase.missingCommitsCheck' to change the level of warnings.");
    eprintln!("The possible behaviours are: ignore, warn, error.");
    eprintln!();
    if level == MissingCommitCheck::Error {
        eprintln!(
            "You can fix this with 'git rebase --edit-todo' and then run 'git rebase --continue'."
        );
        eprintln!("Or you can abort the rebase with 'git rebase --abort'.");
        write_state_atomic(ctx.state_path("dropped"), b"")?;
        return Ok(true);
    }
    Ok(false)
}

fn print_edit_todo_recovery_advice() {
    eprintln!(
        "You can fix this with 'git rebase --edit-todo' and then run 'git rebase --continue'."
    );
    eprintln!("Or you can abort the rebase with 'git rebase --abort'.");
}

fn check_todo_dropped_commits_against_backup(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    new_items: &[RebaseTodoItem],
) -> Result<bool> {
    let Ok(backup) = fs::read_to_string(ctx.state_path("git-rebase-todo.backup")) else {
        return Ok(false);
    };
    let db = ctx.db();
    let mut resolver = make_resolver(ctx, &db);
    let (backup_items, _) = sheet::parse_todo_buffer(
        &backup,
        ctx.state_path("done").exists(),
        comment_char(&ctx.git_dir) as char,
        &mut resolver,
    );
    check_todo_dropped_commits(ctx, hosts, &backup_items, new_items)
}

// ---------------------------------------------------------------------------
// The drive loop
// ---------------------------------------------------------------------------

pub fn pick_commits(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    todo: &mut TodoList,
) -> Result<()> {
    let _ = fs::remove_file(ctx.state_path("message"));
    let _ = fs::remove_file(ctx.state_path("stopped-sha"));
    let _ = fs::remove_file(ctx.state_path("amend"));
    let _ = fs::remove_file(ctx.state_path("patch"));

    while todo.current < todo.items.len() {
        let item = todo.items[todo.current].clone();
        save_todo(ctx, todo, &ctx.db(), false)?;
        if item.command != TodoCommand::Comment {
            todo.done_nr += 1;
            write_state_atomic(ctx.state_path("msgnum"), format!("{}\n", todo.done_nr))?;
            if !opts.quiet {
                let terminator = if opts.verbose { "\n" } else { "\r" };
                eprint!("Rebasing ({}/{}){terminator}", todo.done_nr, todo.total_nr);
            }
        }
        let _ = fs::remove_file(ctx.state_path("author-script"));
        let _ = fs::remove_file(ctx.git_dir.join("MERGE_HEAD"));
        let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
        let _ = fs::remove_file(ctx.git_dir.join("REBASE_HEAD"));

        match item.command {
            TodoCommand::Break => {
                stopped_at_head(ctx, hosts);
                return Ok(());
            }
            TodoCommand::Pick
            | TodoCommand::Reword
            | TodoCommand::Edit
            | TodoCommand::Fixup
            | TodoCommand::Squash => {
                let stop = pick_one_commit(ctx, hosts, opts, todo, &item)?;
                match stop {
                    PickOutcome::Continue => {}
                    PickOutcome::EditStop => return Ok(()),
                    PickOutcome::Fail(code) => return Err(GitError::Exit(code)),
                }
            }
            TodoCommand::Exec => {
                let status = do_exec(ctx, hosts, &item.arg, opts.quiet)?;
                if status != 0 {
                    if opts.reschedule_failed_exec {
                        // Re-insert the exec at the current position.
                        reschedule_current(ctx, hosts, todo, &item)?;
                    }
                    return Err(GitError::Exit(if status == 127 { 1 } else { status }));
                }
                reread_todo_if_changed(ctx, hosts, todo)?;
            }
            TodoCommand::Label => {
                do_label(ctx, &item.arg)?;
            }
            TodoCommand::Reset => {
                if let Err(err) = do_reset(ctx, hosts, opts, &item.arg) {
                    reschedule_current(ctx, hosts, todo, &item)?;
                    return Err(err);
                }
            }
            TodoCommand::Merge => {
                let stop = do_merge(ctx, hosts, opts, todo, &item)?;
                match stop {
                    PickOutcome::Continue => {}
                    PickOutcome::EditStop => return Ok(()),
                    PickOutcome::Fail(code) => return Err(GitError::Exit(code)),
                }
            }
            TodoCommand::UpdateRef => {
                let refname = todo_arg_before_comment(&item.arg).trim().to_string();
                do_update_ref(ctx, &refname)?;
            }
            TodoCommand::Noop | TodoCommand::Drop | TodoCommand::Comment | TodoCommand::Revert => {}
        }

        if todo.current == usize::MAX {
            todo.current = 0;
        } else {
            todo.current += 1;
        }
    }

    finish_rebase(ctx, hosts, opts)
}

fn reschedule_current(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
) -> Result<()> {
    eprintln!("hint: Could not execute the todo command");
    eprintln!("hint: ");
    eprintln!(
        "hint:     {}",
        sheet::render_todo_item(&ctx.db(), item, todo_render_options(ctx, hosts, false, false))
    );
    eprintln!("hint: ");
    eprintln!("hint: It has been rescheduled; To edit the command before continuing, please");
    eprintln!("hint: edit the todo list first:");
    eprintln!("hint: ");
    eprintln!("hint:     git rebase --edit-todo");
    eprintln!("hint:     git rebase --continue");
    // Rewrite the todo file with the current item back at the head.
    save_todo(ctx, todo, &ctx.db(), true)?;
    // Trim the duplicated done line: the item was appended to done by the
    // earlier save_todo, matching git (done keeps the failed attempt).
    Ok(())
}

fn reread_todo_if_changed(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    todo: &mut TodoList,
) -> Result<()> {
    let on_disk = fs::read_to_string(ctx.state_path("git-rebase-todo")).unwrap_or_default();
    let expected = sheet::render_todo_list(
        &ctx.db(),
        &todo.items[todo.current + 1..],
        todo_render_options(ctx, hosts, false, false),
    );
    if on_disk != expected {
        let mut reloaded = read_populate_todo(ctx, &ctx.db())?;
        reloaded.done_nr = todo.done_nr;
        reloaded.total_nr = reloaded.done_nr + sheet::count_commands(&reloaded.items);
        // current will be incremented by the caller loop; compensate.
        *todo = reloaded;
        todo.current = usize::MAX; // sentinel: wraps to 0 on increment
    }
    Ok(())
}

fn stopped_at_head(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) {
    let refs = ctx.refs();
    let db = ctx.db();
    match head_commit_oid(refs) {
        Ok(Some(oid)) => match read_rev_list_commit_record(&db, ctx.format, oid) {
            Ok(record) => {
                eprintln!(
                    "Stopped at {}...  {}",
                    find_unique_abbrev_hex(hosts, &db, &oid),
                    commit_subject(&record.commit.message)
                );
            }
            Err(_) => eprintln!("Stopped at HEAD"),
        },
        _ => eprintln!("Stopped at HEAD"),
    }
}

fn do_exec(ctx: &RebaseContext, hosts: &RebaseHosts<'_>, command: &str, quiet: bool) -> Result<i32> {
    if !quiet {
        eprintln!("Executing: {command}");
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&ctx.worktree_root)
        .env_remove("GIT_CHERRY_PICK_HELP")
        .status()?;
    let code = status.code().unwrap_or(1);
    let dirty = (hosts.short_status)()
        .map(|status| {
            status
                .iter()
                .any(|entry| (entry.index != b' ' || entry.worktree != b' ') && entry.index != b'?')
        })
        .unwrap_or(false);
    if code != 0 {
        eprintln!(
            "warning: execution failed: {command}\n{}You can fix the problem, and then run\n\n  git rebase --continue\n",
            if dirty {
                "and made changes to the index and/or the working tree.\n"
            } else {
                ""
            }
        );
    } else if dirty {
        eprintln!(
            "warning: execution succeeded: {command}\nbut left changes to the index and/or the working tree.\nCommit or stash your changes, and then run\n\n  git rebase --continue\n"
        );
        return Ok(1);
    }
    Ok(code)
}

fn do_label(ctx: &RebaseContext, name: &str) -> Result<()> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("could not read HEAD".into()))?;
    let refname = format!("refs/rewritten/{name}");
    let committer = committer_identity_for_reflog(&ctx.config)?;
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: refname,
        expected: None,
        new: RefTarget::Direct(head),
        reflog: Some(ReflogEntry {
            old_oid: ObjectId::null(ctx.format),
            new_oid: head,
            committer,
            message: format!("rebase (label) {name}").into_bytes(),
        }),
    });
    tx.commit()
}

fn do_reset(ctx: &RebaseContext, hosts: &RebaseHosts<'_>, opts: &MachineOpts, name: &str) -> Result<()> {
    let name = todo_arg_before_comment(name);
    let target = {
        if name == "[new root]" {
            match effective_squash_onto(ctx, opts) {
                Some(oid) => oid,
                None => {
                    let oid = create_squash_onto(ctx)?;
                    write_state_atomic(ctx.state_path("squash-onto"), format!("{oid}\n"))?;
                    oid
                }
            }
        } else {
            let refname = format!("refs/rewritten/{name}");
            let refs = ctx.refs();
            match refs.read_ref(&refname)? {
                Some(RefTarget::Direct(oid)) => oid,
                _ if name.starts_with("refs/")
                    || looks_like_object_name(name)
                    || name.contains('^') =>
                {
                    resolve_reset_target(ctx, name)?
                }
                _ => {
                    eprintln!("error: could not resolve '{name}'");
                    return Err(GitError::Exit(1));
                }
            }
        }
    };
    let db = ctx.db();
    let target_tree = commit_tree_oid(&db, ctx.format, &target)?;
    let overwritten = would_overwrite_untracked(ctx, &db, &target_tree)?;
    if !overwritten.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by reset:"
        );
        for path in &overwritten {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please move or remove them before you reset.");
        return Err(GitError::Exit(1));
    }
    reset_index_and_worktree_to_commit_for_rebase(ctx, hosts, &target)?;
    let refs = ctx.refs();
    let old = head_commit_oid(refs)?.unwrap_or(ObjectId::null(ctx.format));
    let committer = committer_identity_for_reflog(&ctx.config)?;
    detach_head_with_reflog(ctx, old, target, ctx.reflog("reset", Some(name)), committer)
}

/// The active `[new root]` marker commit: `opts.squash_onto` if set, else the
/// squash-onto state file. `reset [new root]` mints this synthetic empty root
/// on the fly (writing only the state file) even for non-`--root` rebases, so
/// both the merge-into-root fast-forward and the pick-as-root-commit paths must
/// consult the file, not just `opts`.
fn effective_squash_onto(ctx: &RebaseContext, opts: &MachineOpts) -> Option<ObjectId> {
    opts.squash_onto.or_else(|| {
        sheet::read_state_line(&ctx.git_dir, "squash-onto")
            .and_then(|raw| ObjectId::from_hex(ctx.format, raw.trim()).ok())
    })
}

/// Mint the synthetic empty root used by `--root` rebases and
/// `reset [new root]`.
pub fn create_squash_onto(ctx: &RebaseContext) -> Result<ObjectId> {
    let ident = commit_identity_from_env("COMMITTER", &ctx.config)?;
    let mut writer = ctx.db();
    crate::create_commit(
        &mut writer,
        crate::CommitCreate {
            tree: ObjectId::empty_tree(ctx.format),
            parents: Vec::new(),
            author: ident.clone(),
            committer: ident,
            message: Vec::new(),
            encoding: None,
            signature: None,
        },
    )
}

fn do_merge(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
) -> Result<PickOutcome> {
    let (labels, oneline) = parse_merge_todo_arg(&item.arg);
    if labels.is_empty() {
        eprintln!("error: nothing to merge: '{}'", item.arg);
        return Ok(PickOutcome::Fail(1));
    }
    let db = ctx.db();
    let mut merge_heads = Vec::new();
    for label in &labels {
        match resolve_merge_label(ctx, &db, label)? {
            Some(oid) => merge_heads.push((label.clone(), oid)),
            None => {
                eprintln!("error: unable to parse '{label}'");
                return Ok(PickOutcome::Fail(1));
            }
        }
    }

    let refs = ctx.refs();
    let head = head_commit_oid(refs)?
        .ok_or_else(|| GitError::Command("cannot merge without HEAD".into()))?;
    let original = match item.oid {
        Some(oid) => Some(read_rev_list_commit_record(&db, ctx.format, oid)?),
        None => None,
    };

    // A "merge" into a "[new root]" is a fast-forward to the merge head. The
    // synthetic root may have been minted on the fly by an earlier `reset [new
    // root]`, which records it only in the squash-onto state file, so consult
    // that too rather than just `opts.squash_onto`.
    if effective_squash_onto(ctx, opts) == Some(head) {
        if merge_heads.len() > 1 {
            eprintln!("error: octopus merge cannot be executed on top of a [new root]");
            return Ok(PickOutcome::Fail(1));
        }
        let target = merge_heads[0].1;
        reset_index_and_worktree_to_commit_for_rebase(ctx, hosts, &target)?;
        let committer = committer_identity_for_reflog(&ctx.config)?;
        detach_head_with_reflog(ctx, head, target, ctx.reflog("merge", None), committer)?;
        return Ok(PickOutcome::Continue);
    }

    if opts.allow_ff
        && let Some(record) = &original
        && record.parents.first() == Some(&head)
        && record.parents[1..]
            .iter()
            .copied()
            .eq(merge_heads.iter().map(|(_, oid)| *oid))
    {
        reset_index_and_worktree_to_commit_for_rebase(ctx, hosts, &record.oid)?;
        let committer = committer_identity_for_reflog(&ctx.config)?;
        detach_head_with_reflog(
            ctx,
            head,
            record.oid,
            format!("{}: fast-forward", ctx.reflog_action).into_bytes(),
            committer,
        )?;
        record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
        if item.flags & sheet::FLAG_EDIT_MERGE_MSG != 0 {
            let result = machine_commit(
                ctx,
                hosts,
                opts,
                MachineCommit {
                    amend: true,
                    edit: true,
                    cleanup_message: true,
                    allow_empty: true,
                    create_root: false,
                    message_file: None,
                    reflog_sub: "merge",
                    original: Some(record),
                },
            )?;
            if let CommitOutcome::Failed(code) = result {
                return Ok(PickOutcome::Fail(code));
            }
            reread_todo_if_changed(ctx, hosts, todo)?;
        }
        return Ok(PickOutcome::Continue);
    }

    if merge_heads.len() > 1 {
        return do_octopus_merge_commit(ctx, hosts, opts, todo, item, &merge_heads, original.as_ref(), oneline);
    }

    let (label, merge_head) = &merge_heads[0];
    if is_ancestor(&ctx.common_git_dir, ctx.format, &db, merge_head, &head)? {
        return Ok(PickOutcome::Continue);
    }

    if let Some(strategy) = &opts.strategy
        && custom_rebase_strategy_needs_external_driver(strategy)
    {
        return do_custom_strategy_merge(
            ctx,
            hosts,
            opts,
            todo,
            item,
            original.as_ref(),
            &labels,
            oneline.as_deref(),
            head,
            *merge_head,
            strategy,
        );
    }

    let merge_tree = commit_tree_oid(&db, ctx.format, merge_head)?;
    let overwritten = would_overwrite_untracked(ctx, &db, &merge_tree)?;
    if !overwritten.is_empty() {
        print_merge_would_overwrite_untracked(&overwritten);
        if let Some(record) = &original {
            write_state_atomic(ctx.git_dir.join("REBASE_HEAD"), format!("{}\n", record.oid))?;
        }
        reschedule_current(ctx, hosts, todo, item)?;
        return Ok(PickOutcome::Fail(1));
    }

    let bases = merge_bases(&ctx.common_git_dir, ctx.format, &db, &head, merge_head)?;
    let base_tree = match bases.first() {
        Some(base) => commit_tree_oid(&db, ctx.format, base)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let head_tree = commit_tree_oid(&db, ctx.format, &head)?;
    let base_map = flatten_tree(&db, ctx.format, &base_tree)?;
    let ours_map = flatten_tree(&db, ctx.format, &head_tree)?;
    let theirs_map = flatten_tree(&db, ctx.format, &merge_tree)?;
    let write_db = ctx.db();
    let (results, conflicts, _) = three_way_merge_trees_inner_with_info_opts(
        &write_db,
        ctx.format,
        &base_map,
        &ours_map,
        &theirs_map,
        "HEAD",
        label,
        "merged common ancestors",
        merge_favor_from_strategy_opts(&opts.strategy_opts),
        ConflictStyle::Merge,
        WsIgnore::EMPTY,
        RenameMergeConfig {
            detect_renames: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: merge_rename_limit_from_config(&ctx.config),
            directory_renames: directory_renames_from_config(&ctx.config),
        },
        hosts.promisor_fetch,
    )?;

    let message = merge_todo_message(ctx, item, original.as_ref(), &labels, oneline.as_deref())?;
    write_state_atomic(ctx.git_dir.join("MERGE_MSG"), &message)?;
    write_state_atomic(ctx.state_path("message"), &message)?;
    write_state_atomic(ctx.git_dir.join("MERGE_HEAD"), format!("{merge_head}\n"))?;

    apply_merge_results(ctx, hosts, &results, &ours_map, !conflicts.is_empty())?;
    if !conflicts.is_empty() {
        let merged_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
        write_state_atomic(ctx.git_dir.join("AUTO_MERGE"), format!("{merged_tree}\n"))?;
        for path in &conflicts {
            let display = String::from_utf8_lossy(path);
            if let Some(advice) = rebase_submodule_conflict_advice(&results, path) {
                eprintln!("Failed to merge submodule {display}");
                eprintln!("CONFLICT (submodule): Merge conflict in {display}");
                eprintln!(
                    "Recursive merging with submodules currently only supports trivial cases."
                );
                eprintln!("Please manually handle the merging of each conflicted submodule.");
                eprintln!("This can be accomplished with the following steps:");
                eprintln!(
                    " - go to submodule ({display}), and either merge commit {}",
                    advice.theirs
                );
                eprintln!("   or update to an existing commit which has merged those changes");
                eprintln!(" - come back to superproject and run:");
                eprintln!("      git add {display}");
                eprintln!("   to record the above merge or update");
                eprintln!(" - resolve any other conflicts in the superproject");
                eprintln!(" - commit the resulting index in the superproject");
            } else {
                println!("Auto-merging {display}");
                println!("CONFLICT (content): Merge conflict in {display}");
            }
        }
        let _ = (hosts.rerere_now)(opts.rerere_autoupdate);
        print_conflict_hints();
        if let Some(record) = &original {
            return stop_with_patch(ctx, hosts, opts, record, item, 1, false);
        }
        return Ok(PickOutcome::Fail(1));
    }

    let tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    create_merge_commit_from_index(ctx, hosts, opts, original.as_ref(), tree, vec![head, *merge_head], &message)?;
    if let Some(record) = &original {
        record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
    }
    if item.flags & sheet::FLAG_EDIT_MERGE_MSG != 0 {
        let result = machine_commit(
            ctx,
            hosts,
            opts,
            MachineCommit {
                amend: true,
                edit: true,
                cleanup_message: true,
                allow_empty: true,
                create_root: false,
                message_file: None,
                reflog_sub: "merge",
                original: original.as_ref(),
            },
        )?;
        if let CommitOutcome::Failed(code) = result {
            return Ok(PickOutcome::Fail(code));
        }
        reread_todo_if_changed(ctx, hosts, todo)?;
    }
    Ok(PickOutcome::Continue)
}

/// `git rebase -s <custom>` shells out to `git-merge-<strategy>`; only ort/
/// recursive/resolve are native here.
fn custom_rebase_strategy_needs_external_driver(strategy: &str) -> bool {
    !matches!(strategy, "ort" | "recursive" | "resolve")
}

fn is_unimplemented_git_core_merge_strategy(strategy: &str) -> bool {
    matches!(strategy, "octopus" | "one-file" | "ours" | "subtree")
}

fn custom_strategy_args(
    opts: &MachineOpts,
    base: ObjectId,
    head: ObjectId,
    merge_head: ObjectId,
) -> Vec<String> {
    let mut command_args: Vec<String> = opts
        .strategy_opts
        .iter()
        .map(|opt| format!("--{opt}"))
        .collect();
    command_args.push(base.to_hex());
    command_args.push("--".to_string());
    command_args.push(head.to_hex());
    command_args.push(merge_head.to_hex());
    command_args
}

fn run_custom_rebase_strategy(
    ctx: &RebaseContext,
    opts: &MachineOpts,
    strategy: &str,
    base: ObjectId,
    head: ObjectId,
    merge_head: ObjectId,
) -> Result<i32> {
    if is_unimplemented_git_core_merge_strategy(strategy) {
        return Err(GitError::Unsupported(format!(
            "merge strategy '{strategy}' is a Git core helper without a native Sley implementation"
        )));
    }
    let status = std::process::Command::new(format!("git-merge-{strategy}"))
        .args(custom_strategy_args(opts, base, head, merge_head))
        .current_dir(&ctx.worktree_root)
        .status()
        .map_err(|err| GitError::Command(format!("failed to run merge strategy: {err}")))?;
    Ok(status.code().unwrap_or(128))
}

#[allow(clippy::too_many_arguments)]
fn pick_one_commit_with_custom_strategy(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
    record: &sley_rev::CommitRecord,
    head: ObjectId,
    parent: Option<ObjectId>,
    is_fixup: bool,
    final_fixup: bool,
    strategy: &str,
) -> Result<PickOutcome> {
    let db = ctx.db();
    let base =
        parent.ok_or_else(|| GitError::Command("custom rebase strategy needs a parent".into()))?;
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let mut message =
        commit_message_for_commit_encoding(&record.commit, &target_encoding).into_owned();
    if opts.signoff && !is_fixup {
        message = (hosts.append_signoff)(message, &commit_signoff_from_env(&ctx.config)?);
    }
    if is_fixup {
        update_squash_messages(ctx, &db, item, record)?;
    }
    write_message_files(ctx, &message, is_fixup, final_fixup)?;
    if !is_fixup {
        write_state_atomic(ctx.state_path("message"), &message)?;
    }
    write_state_atomic(ctx.git_dir.join("MERGE_HEAD"), format!("{}\n", record.oid))?;

    let status = run_custom_rebase_strategy(ctx, opts, strategy, base, head, record.oid)?;
    if status != 0 {
        let _ = (hosts.rerere_now)(opts.rerere_autoupdate);
        print_conflict_hints();
        return stop_with_patch(ctx, hosts, opts, record, item, status, false);
    }

    let tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    let head_tree = commit_tree_oid(&db, ctx.format, &head)?;
    let parent_tree = commit_tree_oid(&db, ctx.format, &base)?;
    let index_unchanged = tree == head_tree;
    let originally_empty = record.commit.tree == parent_tree;
    let mut allow_empty = false;
    if index_unchanged {
        if originally_empty || opts.keep_redundant_commits {
            allow_empty = true;
        } else if opts.drop_redundant_commits {
            eprintln!(
                "dropping {} {} -- patch contents already upstream",
                record.oid,
                commit_subject(&record.commit.message)
            );
            let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
            let _ = fs::remove_file(ctx.git_dir.join("MERGE_HEAD"));
            return Ok(PickOutcome::Continue);
        } else {
            write_state_atomic(
                ctx.git_dir.join("CHERRY_PICK_HEAD"),
                format!("{}\n", record.oid),
            )?;
            eprintln!(
                "The previous cherry-pick is now empty, possibly due to conflict resolution."
            );
            eprintln!("If you wish to commit it anyway, use:");
            eprintln!();
            eprintln!("    git commit --allow-empty");
            eprintln!();
            eprintln!("Otherwise, please use 'git rebase --skip'");
            return stop_with_patch(ctx, hosts, opts, record, item, 1, false);
        }
    }

    let (commit_message_file, amend, edit) = if is_fixup {
        if !final_fixup {
            (Some(ctx.state_path("message-squash")), true, false)
        } else if ctx.state_path("message-fixup").exists() {
            (Some(ctx.state_path("message-fixup")), true, false)
        } else {
            (Some(ctx.state_path("message-squash")), true, true)
        }
    } else {
        (Some(ctx.git_dir.join("MERGE_MSG")), false, false)
    };
    let result = machine_commit(
        ctx,
        hosts,
        opts,
        MachineCommit {
            amend,
            edit: edit || item.command == TodoCommand::Reword,
            cleanup_message: !(is_fixup && !final_fixup),
            allow_empty,
            create_root: false,
            message_file: commit_message_file,
            reflog_sub: command_reflog_name(item.command),
            original: Some(record),
        },
    )?;
    match result {
        CommitOutcome::Committed => {
            record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
        }
        CommitOutcome::Failed(code) => {
            if is_fixup {
                intend_to_amend(ctx)?;
                let squash = fs::read(ctx.state_path("message-squash")).unwrap_or_default();
                write_state_atomic(ctx.state_path("message"), &squash)?;
                write_state_atomic(ctx.git_dir.join("MERGE_MSG"), &squash)?;
            }
            return stop_with_patch(ctx, hosts, opts, record, item, code, false);
        }
    }

    if final_fixup {
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
        let _ = fs::remove_file(ctx.state_path("message-squash"));
        let _ = fs::remove_file(ctx.state_path("current-fixups"));
    }
    if item.command == TodoCommand::Edit {
        eprintln!(
            "Stopped at {}...  {}",
            find_unique_abbrev_hex(hosts, &db, &record.oid),
            item.arg
        );
        return stop_with_patch(ctx, hosts, opts, record, item, 0, true);
    }
    if item.command == TodoCommand::Reword || edit {
        reread_todo_if_changed(ctx, hosts, todo)?;
    }
    Ok(PickOutcome::Continue)
}

#[allow(clippy::too_many_arguments)]
fn do_custom_strategy_merge(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
    original: Option<&sley_rev::CommitRecord>,
    labels: &[String],
    oneline: Option<&str>,
    head: ObjectId,
    merge_head: ObjectId,
    strategy: &str,
) -> Result<PickOutcome> {
    let db = ctx.db();
    let message = merge_todo_message(ctx, item, original, labels, oneline)?;
    write_state_atomic(ctx.git_dir.join("MERGE_MSG"), &message)?;
    write_state_atomic(ctx.state_path("message"), &message)?;
    write_state_atomic(ctx.git_dir.join("MERGE_HEAD"), format!("{merge_head}\n"))?;

    let bases = merge_bases(&ctx.common_git_dir, ctx.format, &db, &head, &merge_head)?;
    let base = bases
        .first()
        .ok_or_else(|| GitError::Command("custom rebase merge strategy needs a base".into()))?;
    let status = run_custom_rebase_strategy(ctx, opts, strategy, *base, head, merge_head)?;
    if status != 0 {
        return Ok(PickOutcome::Fail(status));
    }

    let tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    create_merge_commit_from_index(ctx, hosts, opts, original, tree, vec![head, merge_head], &message)?;
    if let Some(record) = original {
        record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
    }
    if item.flags & sheet::FLAG_EDIT_MERGE_MSG != 0 {
        let result = machine_commit(
            ctx,
            hosts,
            opts,
            MachineCommit {
                amend: true,
                edit: true,
                cleanup_message: true,
                allow_empty: true,
                create_root: false,
                message_file: None,
                reflog_sub: "merge",
                original,
            },
        )?;
        if let CommitOutcome::Failed(code) = result {
            return Ok(PickOutcome::Fail(code));
        }
        reread_todo_if_changed(ctx, hosts, todo)?;
    }
    Ok(PickOutcome::Continue)
}

pub fn todo_arg_before_comment(arg: &str) -> &str {
    arg.split_once(" # ")
        .map(|(left, _)| left.trim())
        .unwrap_or_else(|| arg.trim())
}

fn looks_like_object_name(name: &str) -> bool {
    name.len() >= 7 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn resolve_reset_target(ctx: &RebaseContext, name: &str) -> Result<ObjectId> {
    let db = ctx.db();
    let oid =
        resolve_revision_with_replacement_policy(&ctx.git_dir, ctx.format, name, ctx.replace_objects)?;
    match peel_to_commit(&db, ctx.format, &oid) {
        Ok(commit) => Ok(commit),
        Err(_) => {
            if let Ok(object) = db.read_object(&oid) {
                eprintln!("error: object {oid} is a {}", object.object_type.as_str());
                return Err(GitError::Exit(1));
            }
            Err(GitError::InvalidObject(format!(
                "{name} does not point to a commit"
            )))
        }
    }
}

fn parse_merge_todo_arg(arg: &str) -> (Vec<String>, Option<String>) {
    let (left, oneline) = match arg.split_once(" # ") {
        Some((left, right)) => (left, Some(right.trim().to_string())),
        None => (arg, None),
    };
    (
        left.split_whitespace().map(str::to_string).collect(),
        oneline,
    )
}

fn resolve_merge_label(
    ctx: &RebaseContext,
    db: &FileObjectDatabase,
    label: &str,
) -> Result<Option<ObjectId>> {
    let refs = ctx.refs();
    let rewritten = format!("refs/rewritten/{label}");
    if let Some(RefTarget::Direct(oid)) = refs.read_ref(&rewritten)? {
        return Ok(Some(oid));
    }
    match resolve_revision_with_replacement_policy(&ctx.git_dir, ctx.format, label, ctx.replace_objects)
        .and_then(|oid| peel_to_commit(db, ctx.format, &oid))
    {
        Ok(oid) => Ok(Some(oid)),
        Err(_) => Ok(None),
    }
}

fn merge_todo_message(
    ctx: &RebaseContext,
    _item: &RebaseTodoItem,
    original: Option<&sley_rev::CommitRecord>,
    labels: &[String],
    oneline: Option<&str>,
) -> Result<Vec<u8>> {
    if let Some(record) = original {
        let target_encoding = commit_encoding_config(&ctx.git_dir);
        let author = commit_author_for_commit_encoding(&record.commit, &target_encoding);
        if let Some(script) = sheet::format_author_script(&author) {
            write_state_atomic(ctx.state_path("author-script"), script)?;
        }
        return Ok(
            commit_message_for_commit_encoding(&record.commit, &target_encoding).into_owned(),
        );
    }
    if let Some(oneline) = oneline {
        let mut message = oneline.as_bytes().to_vec();
        message.push(b'\n');
        return Ok(message);
    }
    let message = if labels.len() > 1 {
        format!("Merge branches '{}'\n", labels.join(" "))
    } else {
        format!("Merge branch '{}'\n", labels[0])
    };
    Ok(message.into_bytes())
}

fn create_merge_commit_from_index(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    original: Option<&sley_rev::CommitRecord>,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    message: &[u8],
) -> Result<()> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let author = match read_author_script_identity(ctx)? {
        Some(identity) => identity,
        None => match original {
            Some(record) => {
                commit_author_for_commit_encoding(&record.commit, &target_encoding).into_owned()
            }
            None => commit_identity_from_env("AUTHOR", &ctx.config)?,
        },
    };
    let (author, committer) = rebase_commit_identities(opts, author, &ctx.config)?;
    let encoding = commit_encoding_header_from_config(&ctx.git_dir);
    let mut writer = ctx.db();
    let new_oid = crate::create_commit(
        &mut writer,
        crate::CommitCreate {
            tree,
            parents,
            author,
            committer: committer.clone(),
            message: strip_comment_lines(message, comment_char(&ctx.git_dir)),
            encoding,
            signature: None,
        },
    )?;
    let subject = commit_subject(message);
    detach_head_with_reflog(
        ctx,
        head,
        new_oid,
        ctx.reflog("merge", Some(&subject)),
        committer,
    )?;
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MODE"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    (hosts.run_hook)("post-commit", Vec::new(), None)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn do_octopus_merge_commit(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
    merge_heads: &[(String, ObjectId)],
    original: Option<&sley_rev::CommitRecord>,
    oneline: Option<String>,
) -> Result<PickOutcome> {
    let db = ctx.db();
    let refs = ctx.refs();
    let head = head_commit_oid(refs)?
        .ok_or_else(|| GitError::Command("cannot merge without HEAD".into()))?;
    let mut merged_tree = commit_tree_oid(&db, ctx.format, &head)?;
    let mut parents = vec![head];
    for (label, oid) in merge_heads {
        if is_ancestor(&ctx.common_git_dir, ctx.format, &db, oid, &head)? {
            continue;
        }
        let base = merge_bases(&ctx.common_git_dir, ctx.format, &db, &head, oid)?
            .first()
            .copied()
            .map(|base| commit_tree_oid(&db, ctx.format, &base))
            .transpose()?
            .unwrap_or_else(|| ObjectId::empty_tree(ctx.format));
        let base_map = flatten_tree(&db, ctx.format, &base)?;
        let ours_map = flatten_tree(&db, ctx.format, &merged_tree)?;
        let theirs_tree = commit_tree_oid(&db, ctx.format, oid)?;
        let theirs_map = flatten_tree(&db, ctx.format, &theirs_tree)?;
        let write_db = ctx.db();
        let (results, conflicts, _) = three_way_merge_trees_inner_with_info_opts(
            &write_db,
            ctx.format,
            &base_map,
            &ours_map,
            &theirs_map,
            "HEAD",
            label,
            "merged common ancestors",
            // The plain (no `-X`) merge adapter: favour is never applied here.
            sley_diff_merge::MergeFavor::None,
            ConflictStyle::Merge,
            WsIgnore::EMPTY,
            RenameMergeConfig {
                detect_renames: true,
                rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                rename_limit: merge_rename_limit_from_config(&ctx.config),
                directory_renames: directory_renames_from_config(&ctx.config),
            },
            hosts.promisor_fetch,
        )?;
        apply_merge_results(ctx, hosts, &results, &ours_map, !conflicts.is_empty())?;
        if !conflicts.is_empty() {
            return Ok(PickOutcome::Fail(1));
        }
        merged_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
        parents.push(*oid);
    }
    if parents.len() == 1 {
        return Ok(PickOutcome::Continue);
    }
    let labels: Vec<String> = merge_heads.iter().map(|(label, _)| label.clone()).collect();
    let message = merge_todo_message(ctx, item, original, &labels, oneline.as_deref())?;
    create_merge_commit_from_index(ctx, hosts, opts, original, merged_tree, parents, &message)?;
    if let Some(record) = original {
        record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
    }
    Ok(PickOutcome::Continue)
}

// ---------------------------------------------------------------------------
// Picking one commit
// ---------------------------------------------------------------------------

enum PickOutcome {
    Continue,
    EditStop,
    Fail(i32),
}

#[allow(clippy::too_many_arguments)]
fn pick_one_commit(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    todo: &mut TodoList,
    item: &RebaseTodoItem,
) -> Result<PickOutcome> {
    let db = ctx.db();
    let oid = item
        .oid
        .ok_or_else(|| GitError::Command("pick-like command carries no commit".into()))?;
    let record = read_rev_list_commit_record(&db, ctx.format, oid)?;
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;

    let is_fixup = item.command.is_fixup();
    let final_fixup = is_fixup && !next_is_fixup(todo);
    let create_root = effective_squash_onto(ctx, opts) == Some(head);
    if create_root && is_fixup {
        eprintln!("error: cannot fixup root commit");
        return Ok(PickOutcome::Fail(1));
    }

    let target_encoding = commit_encoding_config(&ctx.git_dir);

    // Write the author script for --continue / commit amending.
    let author = commit_author_for_commit_encoding(&record.commit, &target_encoding);
    if let Some(script) = sheet::format_author_script(&author) {
        write_state_atomic(ctx.state_path("author-script"), script)?;
    }

    let parent = record.parents.first().copied();

    // Fast-forward when the pick's parent is exactly HEAD, or when recreating
    // the root (`--root` with no `--onto`): git treats HEAD == squash_onto as
    // "unborn" (sequencer.c), so a parentless commit fast-forwards onto it and
    // is reused as-is, leaving a no-op `--root` rebase at the original commits.
    let ff_to_head = parent == Some(head);
    let ff_root = create_root && parent.is_none();
    if opts.allow_ff && !is_fixup && (ff_to_head || ff_root) {
        let target_tree = commit_tree_oid(&db, ctx.format, &oid)?;
        let overwritten = would_overwrite_untracked(ctx, &db, &target_tree)?;
        if !overwritten.is_empty() {
            print_merge_would_overwrite_untracked(&overwritten);
            write_state_atomic(ctx.git_dir.join("REBASE_HEAD"), format!("{oid}\n"))?;
            reschedule_current(ctx, hosts, todo, item)?;
            return Ok(PickOutcome::Fail(1));
        }
        reset_index_and_worktree_to_commit_for_rebase(ctx, hosts, &oid)?;
        let committer = committer_identity_for_reflog(&ctx.config)?;
        detach_head_with_reflog(
            ctx,
            head,
            oid,
            format!("{}: fast-forward", ctx.reflog_action).into_bytes(),
            committer,
        )?;
        match item.command {
            TodoCommand::Reword => {
                // Amend the fast-forwarded commit with an edited message.
                let res = machine_commit(
                    ctx,
                    hosts,
                    opts,
                    MachineCommit {
                        amend: true,
                        edit: true,
                        cleanup_message: true,
                        allow_empty: true,
                        create_root: false,
                        message_file: None,
                        reflog_sub: "reword",
                        original: Some(&record),
                    },
                )?;
                if let CommitOutcome::Failed(code) = res {
                    return stop_with_patch(ctx, hosts, opts, &record, item, code, false);
                }
                record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
                reread_todo_if_changed(ctx, hosts, todo)?;
                return Ok(PickOutcome::Continue);
            }
            TodoCommand::Edit => {
                eprintln!(
                    "Stopped at {}...  {}",
                    find_unique_abbrev_hex(hosts, &db, &oid),
                    item.arg
                );
                return stop_with_patch(ctx, hosts, opts, &record, item, 0, true);
            }
            _ => {
                record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
                return Ok(PickOutcome::Continue);
            }
        }
    }

    // Merge the commit's change onto HEAD.
    let parent_tree = match &parent {
        Some(parent) => commit_tree_oid(&db, ctx.format, parent)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let head_tree = commit_tree_oid(&db, ctx.format, &head)?;
    let theirs_tree = record.commit.tree;
    let overwritten = would_overwrite_untracked(ctx, &db, &theirs_tree)?;
    if !overwritten.is_empty() {
        print_merge_would_overwrite_untracked(&overwritten);
        write_state_atomic(ctx.git_dir.join("REBASE_HEAD"), format!("{oid}\n"))?;
        reschedule_current(ctx, hosts, todo, item)?;
        return Ok(PickOutcome::Fail(1));
    }
    if let Some(strategy) = opts.strategy.as_deref().filter(|strategy| {
        custom_rebase_strategy_needs_external_driver(strategy) && parent.is_some()
    }) {
        return pick_one_commit_with_custom_strategy(
            ctx,
            hosts,
            opts,
            todo,
            item,
            &record,
            head,
            parent,
            is_fixup,
            final_fixup,
            strategy,
        );
    }
    let base_map = flatten_tree(&db, ctx.format, &parent_tree)?;
    let ours_map = flatten_tree(&db, ctx.format, &head_tree)?;
    let theirs_map = flatten_tree(&db, ctx.format, &theirs_tree)?;
    let write_db = ctx.db();
    // The conflict-marker label for the picked side is git's `msg.label`:
    // "<short-oid> (<subject>)" (sequencer.c get_message), not the bare subject.
    let theirs_label = format!(
        "{} ({})",
        find_unique_abbrev_hex(hosts, &db, &record.oid),
        commit_subject(&record.commit.message)
    );
    // The base is the parent of the commit being picked, so its diff3 ancestor
    // label is `parent of <msg.label>` (sequencer.c set_replay_opts /
    // `parent_label`). Honour merge.conflictStyle for the picked merge.
    let ancestor_label = format!("parent of {theirs_label}");
    let (results, conflicts, _info) = three_way_merge_trees_inner_with_info_opts_and_path_favor(
        &write_db,
        ctx.format,
        &base_map,
        &ours_map,
        &theirs_map,
        "HEAD",
        &theirs_label,
        &ancestor_label,
        merge_favor_from_strategy_opts(&opts.strategy_opts),
        rebase_merge_conflict_style(&ctx.config),
        merge_ws_ignore_from_strategy_opts(&opts.strategy_opts),
        RenameMergeConfig {
            detect_renames: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: merge_rename_limit_from_config(&ctx.config),
            directory_renames: directory_renames_from_config(&ctx.config),
        },
        None,
        hosts.promisor_fetch,
    )?;

    // Compose the message (fixup/squash machinery).
    let mut message =
        commit_message_for_commit_encoding(&record.commit, &target_encoding).into_owned();
    if opts.signoff && !is_fixup {
        message = (hosts.append_signoff)(message, &commit_signoff_from_env(&ctx.config)?);
    }
    if is_fixup {
        update_squash_messages(ctx, &db, item, &record)?;
    }

    let auto_merged_paths: Vec<Vec<u8>> = results
        .iter()
        .filter_map(|(path, result)| {
            if let MergePathResult::Resolved(Some((mode, oid))) = result
                && ours_map.get(path) != Some(&(*mode, *oid))
                && theirs_map.get(path) != base_map.get(path)
                && ours_map.get(path) != base_map.get(path)
                && ours_map.contains_key(path)
            {
                return Some(path.clone());
            }
            None
        })
        .collect();

    apply_merge_results(ctx, hosts, &results, &ours_map, !conflicts.is_empty())?;

    if !conflicts.is_empty() {
        // Conflict stop.
        let merged_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
        write_state_atomic(ctx.git_dir.join("AUTO_MERGE"), format!("{merged_tree}\n"))?;
        let conflict_set: BTreeSet<Vec<u8>> = conflicts.iter().cloned().collect();
        for path in &auto_merged_paths {
            if !conflict_set.contains(path) {
                println!("Auto-merging {}", String::from_utf8_lossy(path));
            }
        }
        for path in &conflicts {
            let display = String::from_utf8_lossy(path);
            if let Some(advice) = rebase_submodule_conflict_advice(&results, path) {
                eprintln!("Failed to merge submodule {display}");
                eprintln!("CONFLICT (submodule): Merge conflict in {display}");
                eprintln!(
                    "Recursive merging with submodules currently only supports trivial cases."
                );
                eprintln!("Please manually handle the merging of each conflicted submodule.");
                eprintln!("This can be accomplished with the following steps:");
                eprintln!(
                    " - go to submodule ({display}), and either merge commit {}",
                    advice.theirs
                );
                eprintln!("   or update to an existing commit which has merged those changes");
                eprintln!(" - come back to superproject and run:");
                eprintln!("      git add {display}");
                eprintln!("   to record the above merge or update");
                eprintln!(" - resolve any other conflicts in the superproject");
                eprintln!(" - commit the resulting index in the superproject");
            } else {
                println!("Auto-merging {display}");
                println!("CONFLICT (content): Merge conflict in {display}");
            }
        }

        // MERGE_MSG with the conflicts comment block.
        let mut merge_msg = message;
        if !merge_msg.ends_with(b"\n") {
            merge_msg.push(b'\n');
        }
        let comment = comment_char(&ctx.git_dir);
        merge_msg.push(b'\n');
        merge_msg.push(comment);
        merge_msg.extend_from_slice(b" Conflicts:\n");
        for path in &conflicts {
            merge_msg.push(comment);
            merge_msg.push(b'\t');
            merge_msg.extend_from_slice(path);
            merge_msg.push(b'\n');
        }
        if is_fixup {
            // error_failed_squash: message file gets the squash message.
            let squash = fs::read(ctx.state_path("message-squash")).unwrap_or_default();
            write_state_atomic(ctx.state_path("message"), &squash)?;
            write_state_atomic(ctx.git_dir.join("MERGE_MSG"), &squash)?;
            intend_to_amend(ctx)?;
        } else {
            write_state_atomic(ctx.git_dir.join("MERGE_MSG"), &merge_msg)?;
            write_state_atomic(ctx.state_path("message"), &merge_msg)?;
        }

        // Record the conflict in the rerere database and, if a resolution is
        // known, replay it (staging it when rerere.autoUpdate / --rerere-
        // autoupdate is in effect).
        let _ = (hosts.rerere_now)(opts.rerere_autoupdate);

        eprintln!(
            "error: could not apply {}... {}",
            find_unique_abbrev_hex(hosts, &db, &oid),
            commit_subject(&record.commit.message)
        );
        print_conflict_hints();
        return stop_with_patch(ctx, hosts, opts, &record, item, 1, false);
    }

    for path in &auto_merged_paths {
        println!("Auto-merging {}", String::from_utf8_lossy(path));
    }

    let merged_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;

    // Empty handling.
    let index_unchanged = merged_tree == head_tree;
    let originally_empty = theirs_tree == parent_tree;
    let mut allow_empty = false;
    if index_unchanged {
        if originally_empty {
            allow_empty = true; // rebase always allows originally-empty commits
        } else if opts.keep_redundant_commits {
            allow_empty = true;
        } else if opts.drop_redundant_commits {
            eprintln!(
                "dropping {} {} -- patch contents already upstream",
                oid,
                commit_subject(&record.commit.message)
            );
            let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
            return Ok(PickOutcome::Continue);
        } else {
            // EMPTY_STOP: try to commit without --allow-empty; it fails with
            // the "previous cherry-pick is now empty" advice.
            write_state_atomic(ctx.git_dir.join("CHERRY_PICK_HEAD"), format!("{oid}\n"))?;
            write_message_files(ctx, &message, is_fixup, final_fixup)?;
            eprintln!(
                "The previous cherry-pick is now empty, possibly due to conflict resolution."
            );
            eprintln!("If you wish to commit it anyway, use:");
            eprintln!();
            eprintln!("    git commit --allow-empty");
            eprintln!();
            eprintln!("Otherwise, please use 'git rebase --skip'");
            return stop_with_patch(ctx, hosts, opts, &record, item, 1, false);
        }
    }

    // Commit.
    write_message_files(ctx, &message, is_fixup, final_fixup)?;
    let (commit_message_file, amend, edit) = if is_fixup {
        if !final_fixup {
            (Some(ctx.state_path("message-squash")), true, false)
        } else if ctx.state_path("message-fixup").exists() {
            (Some(ctx.state_path("message-fixup")), true, false)
        } else {
            (Some(ctx.state_path("message-squash")), true, true)
        }
    } else {
        (Some(ctx.git_dir.join("MERGE_MSG")), false, false)
    };
    if item.command == TodoCommand::Reword && !is_fixup {
        let result = machine_commit(
            ctx,
            hosts,
            opts,
            MachineCommit {
                amend,
                edit,
                cleanup_message: true,
                allow_empty,
                create_root,
                message_file: commit_message_file,
                reflog_sub: command_reflog_name(item.command),
                original: Some(&record),
            },
        )?;
        if let CommitOutcome::Failed(code) = result {
            return stop_with_patch(ctx, hosts, opts, &record, item, code, false);
        }
        let result = machine_commit(
            ctx,
            hosts,
            opts,
            MachineCommit {
                amend: true,
                edit: true,
                cleanup_message: true,
                allow_empty: true,
                create_root: false,
                message_file: None,
                reflog_sub: command_reflog_name(item.command),
                original: Some(&record),
            },
        )?;
        match result {
            CommitOutcome::Committed => {
                record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
                reread_todo_if_changed(ctx, hosts, todo)?;
                return Ok(PickOutcome::Continue);
            }
            CommitOutcome::Failed(code) => {
                return stop_with_patch(ctx, hosts, opts, &record, item, code, false);
            }
        }
    }
    let result = machine_commit(
        ctx,
        hosts,
        opts,
        MachineCommit {
            amend,
            edit,
            cleanup_message: !(is_fixup && !final_fixup),
            allow_empty,
            create_root,
            message_file: commit_message_file,
            reflog_sub: command_reflog_name(item.command),
            original: Some(&record),
        },
    )?;
    match result {
        CommitOutcome::Committed => {
            record_rewritten(ctx, &record.oid, next_command_after_current(todo))?;
        }
        CommitOutcome::Failed(code) => {
            if is_fixup {
                intend_to_amend(ctx)?;
                let squash = fs::read(ctx.state_path("message-squash")).unwrap_or_default();
                write_state_atomic(ctx.state_path("message"), &squash)?;
                write_state_atomic(ctx.git_dir.join("MERGE_MSG"), &squash)?;
            }
            return stop_with_patch(ctx, hosts, opts, &record, item, code, false);
        }
    }

    if final_fixup {
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
        let _ = fs::remove_file(ctx.state_path("message-squash"));
        let _ = fs::remove_file(ctx.state_path("current-fixups"));
    }

    if item.command == TodoCommand::Edit {
        eprintln!(
            "Stopped at {}...  {}",
            find_unique_abbrev_hex(hosts, &db, &oid),
            item.arg
        );
        return stop_with_patch(ctx, hosts, opts, &record, item, 0, true);
    }

    if item.command == TodoCommand::Reword || edit {
        reread_todo_if_changed(ctx, hosts, todo)?;
    }
    Ok(PickOutcome::Continue)
}

/// `merge.conflictStyle` for a rebase pick's 3-way merge (honouring `-c`
/// overrides). diff3 and zdiff3 both add the `|||||||` base section; sley does
/// not yet distinguish the zealous variant.
fn rebase_merge_conflict_style(config: &GitConfig) -> ConflictStyle {
    config
        .get("merge", None, "conflictstyle")
        .map(str::to_string)
        .map(|value| match value.as_str() {
            "diff3" | "zdiff3" => ConflictStyle::Diff3,
            _ => ConflictStyle::Merge,
        })
        .unwrap_or(ConflictStyle::Merge)
}

struct RebaseSubmoduleConflictAdvice {
    theirs: String,
}

fn rebase_submodule_conflict_advice(
    results: &MergePathResults,
    path: &[u8],
) -> Option<RebaseSubmoduleConflictAdvice> {
    let MergePathResult::Conflict {
        base, ours, theirs, ..
    } = results.get(path)?
    else {
        return None;
    };
    if ![base, ours, theirs]
        .into_iter()
        .flatten()
        .any(|(mode, _)| sley_index::is_gitlink(*mode))
    {
        return None;
    }
    let (_, theirs_oid) = theirs.as_ref()?;
    Some(RebaseSubmoduleConflictAdvice {
        theirs: short_oid(theirs_oid),
    })
}

fn short_oid(oid: &ObjectId) -> String {
    oid.to_hex()[..oid.abbrev_hex_len(7)].to_string()
}

fn command_reflog_name(command: TodoCommand) -> &'static str {
    match command {
        TodoCommand::Pick => "pick",
        TodoCommand::Reword => "reword",
        TodoCommand::Edit => "edit",
        TodoCommand::Fixup => "fixup",
        TodoCommand::Squash => "squash",
        _ => "pick",
    }
}

fn next_is_fixup(todo: &TodoList) -> bool {
    todo.items[todo.current + 1..]
        .iter()
        .find(|item| item.command != TodoCommand::Comment)
        .is_some_and(|item| item.command.is_fixup())
}

fn next_command_after_current(todo: &TodoList) -> Option<TodoCommand> {
    todo.items[todo.current + 1..]
        .iter()
        .find(|item| item.command != TodoCommand::Comment)
        .map(|item| item.command)
}

fn first_todo_command(todo: &TodoList) -> Option<TodoCommand> {
    todo.items
        .iter()
        .find(|item| item.command != TodoCommand::Comment)
        .map(|item| item.command)
}

fn write_message_files(
    ctx: &RebaseContext,
    message: &[u8],
    is_fixup: bool,
    _final_fixup: bool,
) -> Result<()> {
    if !is_fixup {
        write_state_atomic(ctx.git_dir.join("MERGE_MSG"), message)?;
    }
    Ok(())
}

fn apply_merge_results(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    results: &MergePathResults,
    ours_map: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    with_conflicts: bool,
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    let mut old_index = if index_path.is_file() {
        Index::parse(&fs::read(&index_path)?, ctx.format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    if old_index.is_sparse() {
        for entry in &mut old_index.entries {
            if entry.mode == sley_index::SPARSE_DIR_MODE && entry.path.as_bytes().ends_with(b"/") {
                entry.set_skip_worktree(true);
            }
        }
        sley_worktree::expand_sparse_index_view(&mut old_index, &ctx.db(), ctx.format)?;
    }
    let old_entries: BTreeMap<Vec<u8>, IndexEntry> = old_index
        .entries
        .into_iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect();

    // Materialize the worktree BEFORE building the index so resolved stage-0
    // entries can record the on-disk stat (git refreshes merged results via
    // fill_stat_cache_info; a zeroed stat makes diff-files report them dirty).
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = if sley_index::is_gitlink(*mode) {
                        Vec::new()
                    } else {
                        merge_read_blob_with_fetch(&ctx.db(), oid, hosts.promisor_fetch)?
                    };
                    merge_write_worktree_file(&ctx.worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => {
                if ours_map.contains_key(path) {
                    merge_remove_worktree_file(&ctx.worktree_root, path)?;
                }
            }
            MergePathResult::Conflict { worktree, .. } => {
                if with_conflicts {
                    match worktree {
                        Some((mode, content)) => {
                            merge_write_worktree_file(&ctx.worktree_root, path, content, *mode)?
                        }
                        None => merge_remove_worktree_file(&ctx.worktree_root, path)?,
                    }
                }
            }
        }
    }

    let mut entries = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                let mut entry = match old_entries.get(path) {
                    Some(old) if old.mode == *mode && old.oid == *oid => old.clone(),
                    _ => merge_index_entry(path, *mode, *oid, 0),
                };
                if !sley_index::is_gitlink(*mode)
                    && entry.mtime_seconds == 0
                    && entry.ctime_seconds == 0
                    && let Ok(rel) = std::str::from_utf8(path)
                    && let Ok(metadata) = fs::symlink_metadata(ctx.worktree_root.join(rel))
                {
                    sley_worktree::fill_index_entry_stat_cache(&mut entry, &metadata);
                }
                entries.push(entry);
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict {
                base, ours, theirs, ..
            } => {
                if let Some((mode, oid)) = base {
                    entries.push(merge_index_entry(path, *mode, *oid, 1));
                }
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 2));
                }
                if let Some((mode, oid)) = theirs {
                    entries.push(merge_index_entry(path, *mode, *oid, 3));
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
    });
    let mut index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    index.upgrade_version_for_flags();
    write_state_atomic(index_path, index.write(ctx.format)?)?;
    Ok(())
}

fn index_entry_stage(entry: &IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

fn print_conflict_hints() {
    eprintln!("hint: Resolve all conflicts manually, mark them as resolved with");
    eprintln!("hint: \"git add/rm <conflicted_files>\", then run \"git rebase --continue\".");
    eprintln!("hint: You can instead skip this commit: run \"git rebase --skip\".");
    eprintln!(
        "hint: To abort and get back to the state before \"git rebase\", run \"git rebase --abort\"."
    );
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

fn intend_to_amend(ctx: &RebaseContext) -> Result<()> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    write_state_atomic(ctx.state_path("amend"), format!("{head}\n"))?;
    Ok(())
}

/// `error_with_patch` / `make_patch`: record stop state and exit.
fn stop_with_patch(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    record: &sley_rev::CommitRecord,
    _item: &RebaseTodoItem,
    exit_code: i32,
    to_amend: bool,
) -> Result<PickOutcome> {
    write_state_atomic(ctx.state_path("stopped-sha"), format!("{}\n", record.oid))?;
    write_state_atomic(ctx.git_dir.join("REBASE_HEAD"), format!("{}\n", record.oid))?;

    // Write the patch file: diff of the commit against its first parent.
    let parent_tree = match record.parents.first() {
        Some(parent) => commit_tree_oid(&ctx.db(), ctx.format, parent)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let patch = (hosts.tree_patch)(&parent_tree, &record.commit.tree)?;
    write_state_atomic(ctx.state_path("patch"), patch)?;

    if to_amend {
        // An `edit` command has already created the rewritten commit.  Resume
        // must amend that commit with its *current* message, not the original
        // pre-rebase message.  The distinction is observable with options that
        // transform the message while picking (notably `rebase --signoff`):
        // saving `record.commit.message` here made the subsequent `--continue`
        // silently discard the trailer.  Reading HEAD also preserves any
        // editor-driven message change made by the commit step itself.
        let head = head_commit_oid(ctx.refs())?
            .ok_or_else(|| GitError::Command("cannot read HEAD after edit".into()))?;
        let head_record = read_rev_list_commit_record(&ctx.db(), ctx.format, head)?;
        let mut message = head_record.commit.message;
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        write_state_atomic(ctx.state_path("message"), message)?;
    } else if !ctx.state_path("message").exists() {
        let mut message = record.commit.message.clone();
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        write_state_atomic(ctx.state_path("message"), message)?;
    }

    if to_amend {
        intend_to_amend(ctx)?;
        let sign_opt = opts.gpg_sign.as_ref().map(|key| {
            if key.is_empty() {
                " -S".to_string()
            } else {
                format!(" '-S{key}'")
            }
        });
        eprintln!("You can amend the commit now, with");
        eprintln!();
        eprintln!("  git commit --amend{} ", sign_opt.as_deref().unwrap_or(""));
        eprintln!();
        eprintln!("Once you are satisfied with your changes, run");
        eprintln!();
        eprintln!("  git rebase --continue");
        return Ok(PickOutcome::EditStop);
    }
    if exit_code != 0 {
        // git error_with_patch prints the parsed commit subject (`%.*s`,
        // subject_len/subject), not the raw todo arg (which carries the `# `
        // prefix `pick <oid> # <subject>`).
        eprintln!(
            "Could not apply {}... {}",
            find_unique_abbrev_hex(hosts, &ctx.db(), &record.oid),
            commit_subject(&record.commit.message)
        );
        return Ok(PickOutcome::Fail(exit_code));
    }
    Ok(PickOutcome::EditStop)
}

// ---------------------------------------------------------------------------
// fixup / squash message machinery
// ---------------------------------------------------------------------------

fn current_fixup_count(ctx: &RebaseContext) -> usize {
    fs::read_to_string(ctx.state_path("current-fixups"))
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

fn commented_lines(text: &[u8], comment: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.split_inclusive(|&b| b == b'\n') {
        let content = if line.ends_with(b"\n") {
            &line[..line.len() - 1]
        } else {
            line
        };
        if content.is_empty() {
            out.extend_from_slice(comment);
        } else {
            out.extend_from_slice(comment);
            out.push(b' ');
            out.extend_from_slice(content);
        }
        out.push(b'\n');
    }
    out
}

/// `commit_subject_length`: bytes of the subject paragraph (up to and including
/// the blank line that ends it), so `amend!`/`fixup!`/`squash!` subjects can be
/// commented out while the body stays verbatim.
fn commit_subject_length(body: &[u8]) -> usize {
    let mut p = 0usize;
    while p < body.len() {
        // skip_blank_lines: if the current line is blank, the subject ends here.
        let next = skip_blank_lines(&body[p..]) + p;
        if next != p {
            break;
        }
        // advance past this (non-blank) line.
        match body[p..].iter().position(|&b| b == b'\n') {
            Some(off) => p += off + 1,
            None => return body.len(),
        }
    }
    p
}

/// `skip_blank_lines`: return the offset past any leading all-whitespace lines.
fn skip_blank_lines(buf: &[u8]) -> usize {
    let mut p = 0usize;
    loop {
        let eol = buf[p..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|off| p + off)
            .unwrap_or(buf.len());
        if buf[p..eol].iter().any(|&b| !b.is_ascii_whitespace()) {
            return p;
        }
        if eol >= buf.len() {
            return buf.len();
        }
        p = eol + 1;
    }
}

/// `seen_squash`: the current fixup chain contains a `squash` command.
fn seen_squash(ctx: &RebaseContext) -> bool {
    fs::read_to_string(ctx.state_path("current-fixups"))
        .map(|text| text.starts_with("squash") || text.contains("\nsquash"))
        .unwrap_or(false)
}

/// `is_fixup_flag`: a `fixup -C` / `fixup -c` (replaces the prior message).
fn is_fixup_flag(command: TodoCommand, flags: u8) -> bool {
    command == TodoCommand::Fixup
        && (flags & sheet::FLAG_REPLACE_FIXUP_MSG != 0 || flags & sheet::FLAG_EDIT_FIXUP_MSG != 0)
}

/// `update_squash_message_for_fixup`: when a `fixup -C/-c` follows earlier
/// messages, re-comment any still-uncommented prior commit message so only the
/// replacing message survives. Mirrors sequencer.c by rewriting the
/// "This is the …th commit message:" headers to their "will be skipped" form
/// and commenting the bodies they introduce.
fn update_squash_message_for_fixup(msg: &[u8], comment: &str) -> Vec<u8> {
    let kept_first = format!("{comment} This is the 1st commit message:");
    let skip_first = format!("{comment} The 1st commit message will be skipped:");
    let max_message = msg.iter().filter(|&&b| b == b'\n').count() + 2;
    let nth_markers = (2..=max_message)
        .map(|n| {
            (
                format!("{comment} This is the commit message #{n}:"),
                format!("{comment} The commit message #{n} will be skipped:"),
            )
        })
        .collect::<Vec<_>>();
    let mut out = Vec::with_capacity(msg.len());
    let mut commenting = false;
    let mut idx = 1usize;
    for line in msg.split_inclusive(|&b| b == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let (kept_nth, skip_nth) = &nth_markers[idx - 1];
        if body == kept_first.as_bytes() {
            out.extend_from_slice(skip_first.as_bytes());
            out.push(b'\n');
            commenting = true;
            continue;
        }
        if body == kept_nth.as_bytes() {
            out.extend_from_slice(skip_nth.as_bytes());
            out.push(b'\n');
            idx += 1;
            commenting = true;
            continue;
        }
        if body == skip_first.as_bytes() || body == skip_nth.as_bytes() {
            if body == skip_nth.as_bytes() {
                idx += 1;
            }
            out.extend_from_slice(line);
            commenting = false;
            continue;
        }
        if commenting {
            // Comment out the message body, but leave blank lines untouched.
            if body.is_empty() {
                out.push(b'\n');
            } else if body.starts_with(comment.as_bytes()) {
                out.extend_from_slice(line);
            } else {
                out.extend_from_slice(comment.as_bytes());
                out.push(b' ');
                out.extend_from_slice(line);
            }
        } else {
            out.extend_from_slice(line);
        }
    }
    out
}

fn remove_last_squash_message_section(
    msg: &[u8],
    remaining_messages: usize,
    comment: &str,
) -> Vec<u8> {
    let mut text = String::from_utf8_lossy(msg).into_owned();

    if let Some(first_newline) = text.find('\n') {
        let header = format!("{comment} This is a combination of ");
        if text.starts_with(&header) {
            let replacement =
                format!("{comment} This is a combination of {remaining_messages} commits.");
            text.replace_range(..first_newline, &replacement);
        }
    }

    let markers = [
        format!("\n{comment} This is the commit message #"),
        format!("\n{comment} The commit message #"),
    ];
    if let Some(index) = markers.iter().filter_map(|marker| text.rfind(marker)).max() {
        text.truncate(index + 1);
    }
    text.into_bytes()
}

/// Append the replacing/squashed message body (`append_squash_message`):
/// the "commit message #N" header plus the fixup commit's body. For
/// `fixup -C/-c` the body replaces the message, so it is added verbatim
/// (uncommented) and may be persisted to `message-fixup`.
fn append_squash_message(
    ctx: &RebaseContext,
    buf: &mut Vec<u8>,
    body: &[u8],
    command: TodoCommand,
    flags: u8,
    comment: &str,
    count: usize,
) -> Result<()> {
    // `amend!` subjects (and fixup!/squash! when squashing) get their subject
    // commented out.
    let commented_len = if body.starts_with(b"amend!")
        || ((command == TodoCommand::Squash || seen_squash(ctx))
            && (body.starts_with(b"squash!") || body.starts_with(b"fixup!")))
    {
        commit_subject_length(body)
    } else {
        0
    };
    buf.push(b'\n');
    buf.extend_from_slice(
        format!(
            "{comment} This is the commit message #{}:\n\n",
            count + 2
        )
        .as_bytes(),
    );
    buf.extend_from_slice(&commented_lines(&body[..commented_len], comment.as_bytes()));
    let fixup_off = buf.len();
    buf.extend_from_slice(&body[commented_len..]);

    if is_fixup_flag(command, flags) && !seen_squash(ctx) {
        if (flags & sheet::FLAG_REPLACE_FIXUP_MSG != 0)
            && (ctx.state_path("message-fixup").exists()
                || !ctx.state_path("message-squash").exists())
        {
            let fixup_msg = &buf[fixup_off + skip_blank_lines(&buf[fixup_off..])..];
            write_state_atomic(ctx.state_path("message-fixup"), fixup_msg)?;
        } else {
            let _ = fs::remove_file(ctx.state_path("message-fixup"));
        }
    } else {
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
    }
    Ok(())
}

/// `update_squash_messages`: build the combined `message-squash` (and, for plain
/// fixup chains, `message-fixup`). Handles `squash`, plain `fixup`, and
/// `fixup -C`/`-c` (`FLAG_REPLACE_FIXUP_MSG` / `FLAG_EDIT_FIXUP_MSG`).
fn update_squash_messages(
    ctx: &RebaseContext,
    db: &FileObjectDatabase,
    item: &RebaseTodoItem,
    record: &sley_rev::CommitRecord,
) -> Result<()> {
    let comment = commit_comment_string(&ctx.git_dir);
    let comment_str = comment.as_str();
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let count = current_fixup_count(ctx);
    let flagged = is_fixup_flag(item.command, item.flags);
    let mut buf: Vec<u8>;
    if count > 0 {
        let existing = fs::read(ctx.state_path("message-squash"))?;
        // Replace the first line (the combination header).
        let eol = if existing.starts_with(comment.as_bytes()) {
            existing
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(existing.len())
        } else {
            0
        };
        buf = format!(
            "{comment_str} This is a combination of {} commits.",
            count + 2
        )
        .into_bytes();
        buf.extend_from_slice(&existing[eol..]);
        if flagged && !seen_squash(ctx) {
            buf = update_squash_message_for_fixup(&buf, &comment);
        }
    } else {
        let refs = ctx.refs();
        let head = head_commit_oid(refs)?
            .ok_or_else(|| GitError::Command("need a HEAD to fixup".into()))?;
        let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
        let head_body =
            commit_message_for_commit_encoding(&head_record.commit, &target_encoding).into_owned();
        // Plain fixup (no flag) seeds message-fixup with HEAD's body.
        if item.command == TodoCommand::Fixup && item.flags == 0 {
            write_state_atomic(ctx.state_path("message-fixup"), &head_body)?;
        }
        buf = format!("{comment_str} This is a combination of 2 commits.\n").into_bytes();
        if flagged {
            buf.extend_from_slice(
                format!("{comment_str} The 1st commit message will be skipped:\n\n").as_bytes(),
            );
            buf.extend_from_slice(&commented_lines(&head_body, comment.as_bytes()));
        } else {
            buf.extend_from_slice(
                format!("{comment_str} This is the 1st commit message:\n\n").as_bytes(),
            );
            buf.extend_from_slice(&head_body);
        }
    }

    let body = commit_message_for_commit_encoding(&record.commit, &target_encoding);
    if item.command == TodoCommand::Squash || flagged {
        append_squash_message(
            ctx,
            &mut buf,
            &body,
            item.command,
            item.flags,
            &comment,
            count,
        )?;
    } else {
        // Plain fixup: the message is skipped.
        buf.push(b'\n');
        buf.extend_from_slice(
            format!(
                "{comment_str} The commit message #{} will be skipped:\n\n",
                count + 2
            )
            .as_bytes(),
        );
        buf.extend_from_slice(&commented_lines(&body, comment.as_bytes()));
    }
    write_state_atomic(ctx.state_path("message-squash"), &buf)?;

    // Append to current-fixups.
    let mut fixups = fs::read_to_string(ctx.state_path("current-fixups")).unwrap_or_default();
    if !fixups.is_empty() && !fixups.ends_with('\n') {
        fixups.push('\n');
    }
    fixups.push_str(item.command.as_str());
    fixups.push(' ');
    fixups.push_str(&record.oid.to_hex());
    fixups.push('\n');
    write_state_atomic(
        ctx.state_path("current-fixups"),
        fixups.trim_end_matches('\n'),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Native `git commit` for the machine
// ---------------------------------------------------------------------------

struct MachineCommit<'a> {
    amend: bool,
    edit: bool,
    cleanup_message: bool,
    allow_empty: bool,
    create_root: bool,
    /// Seed message file (MERGE_MSG / message-squash / message-fixup); `None`
    /// amends with HEAD's message.
    message_file: Option<PathBuf>,
    reflog_sub: &'a str,
    /// The original commit being replayed (authorship source).
    original: Option<&'a sley_rev::CommitRecord>,
}

enum CommitOutcome {
    Committed,
    Failed(i32),
}

fn machine_commit(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    commit: MachineCommit<'_>,
) -> Result<CommitOutcome> {
    let db = ctx.db();
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let head_record = read_rev_list_commit_record(&db, ctx.format, head)?;

    // A missing seed file is corruption (the machine wrote it before stopping):
    // error out instead of silently committing an empty message.
    let seed = match &commit.message_file {
        Some(path) => fs::read(path)?,
        None => head_record.commit.message.clone(),
    };

    let editmsg = ctx.git_dir.join("COMMIT_EDITMSG");
    let comment_string = commit_comment_string(&ctx.git_dir);
    let template = if commit.edit {
        let mut template = seed;
        if !template.ends_with(b"\n") {
            template.push(b'\n');
        }
        // git seeds COMMIT_EDITMSG with the message followed by a blank line and
        // the standard help comment block; the trailing blank line is what lets
        // an appended line (e.g. `fixup -c`'s amended subject) land in its own
        // paragraph after stripspace.
        template.push(b'\n');
        template.extend_from_slice(
            format!(
                "{comment_string} Please enter the commit message for your changes. Lines starting\n{comment_string} with '{comment_string}' will be ignored, and an empty message aborts the commit.\n"
            )
            .as_bytes(),
        );
        template.extend_from_slice(&(hosts.editor_status_block)(&comment_string, commit.amend)?);
        template
    } else {
        seed
    };
    let prepare_source_is_head = commit.amend && commit.message_file.is_none();
    let prepare_source_is_merge = ctx.git_dir.join("CHERRY_PICK_HEAD").is_file();
    let mut message = (hosts.prepare_commit_message)(
        &editmsg,
        template,
        prepare_source_is_head,
        prepare_source_is_merge,
        commit.edit,
    )?;
    if commit.edit {
        message = strip_comment_string_lines(&message, comment_string.as_bytes());
    } else if commit.cleanup_message {
        // verbatim, but the seed files for non-edit commits never carry
        // comments except the conflicts block which only exists when editing.
        message = strip_comment_lines(&message, comment_char(&ctx.git_dir));
    }
    // An editor that clears the post-cleanup message aborts the commit. Plain
    // machine picks, however, preserve an original empty message without
    // requiring an explicit --allow-empty-message option.
    if commit.edit && message.iter().all(|b| b.is_ascii_whitespace()) {
        eprintln!("Aborting commit due to empty commit message.");
        return Ok(CommitOutcome::Failed(1));
    }

    let tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    let old_tree_for_summary = if commit.reflog_sub == "continue" && !opts.quiet {
        Some(if commit.amend {
            head_record.commit.tree
        } else {
            commit_tree_oid(&db, ctx.format, &head)?
        })
    } else {
        None
    };
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let (parents, author) = if commit.create_root {
        let author = match read_author_script_identity(ctx)? {
            Some(identity) => identity,
            None => match commit.original {
                Some(record) => {
                    commit_author_for_commit_encoding(&record.commit, &target_encoding).into_owned()
                }
                None => commit_identity_from_env("AUTHOR", &ctx.config)?,
            },
        };
        (Vec::new(), author)
    } else if commit.amend {
        let author = head_record.commit.author.clone();
        (head_record.commit.parents, author)
    } else {
        let author = match read_author_script_identity(ctx)? {
            Some(identity) => identity,
            None => match commit.original {
                Some(record) => {
                    commit_author_for_commit_encoding(&record.commit, &target_encoding).into_owned()
                }
                None => commit_identity_from_env("AUTHOR", &ctx.config)?,
            },
        };
        (vec![head], author)
    };

    if !commit.amend && !commit.create_root && !commit.allow_empty {
        let parent_tree = commit_tree_oid(&db, ctx.format, &head)?;
        if tree == parent_tree {
            return Ok(CommitOutcome::Failed(1));
        }
    }

    // Apply `--reset-author-date`/`--ignore-date` and
    // `--committer-date-is-author-date`, mirroring sequencer.c's
    // try_to_commit. `ignore_date` rewrites the author date to "now"; the
    // committer date is then either "now" (`ignore_date`), the author's date
    // (`committer_date_is_author_date` without `ignore_date`), or the
    // environment's committer date.
    let (author, committer) = rebase_commit_identities(opts, author, &ctx.config)?;
    let encoding = commit_encoding_header_from_config(&ctx.git_dir);
    let signature = rebase_commit_signature(
        ctx,
        hosts,
        opts,
        tree,
        &parents,
        &author,
        &committer,
        &message,
        encoding.clone(),
    )?;
    let mut writer = ctx.db();
    let new_oid = crate::create_commit(
        &mut writer,
        crate::CommitCreate {
            tree,
            parents,
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding,
            signature,
        },
    )?;

    let subject = commit_subject(&message);
    let reflog_message = if commit.reflog_sub.is_empty() {
        format!("{}: {subject}", ctx.reflog_action).into_bytes()
    } else {
        ctx.reflog(commit.reflog_sub, Some(&subject))
    };
    detach_head_with_reflog(ctx, head, new_oid, reflog_message, committer)?;

    // Record any rerere resolution for the just-committed conflict (matches
    // git invoking rerere() on commit), so a later identical conflict replays.
    let _ = (hosts.rerere_record_resolved)();

    // Post-commit cleanup.
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    (hosts.run_hook)("post-commit", Vec::new(), None)?;

    if let Some(old_tree) = old_tree_for_summary {
        (hosts.print_continue_summary)(&new_oid, &message, old_tree, tree)?;
    }

    Ok(CommitOutcome::Committed)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rebase_commit_signature(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
    tree: ObjectId,
    parents: &[ObjectId],
    author: &[u8],
    committer: &[u8],
    message: &[u8],
    encoding: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    let _ = ctx;
    (hosts.commit_signature)(tree, parents, author, committer, message, encoding, opts)
}

pub(crate) fn rebase_commit_identities(
    opts: &MachineOpts,
    author: Vec<u8>,
    config: &GitConfig,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let now = opts.ignore_date.then(rebase_now_date);
    let author = match &now {
        Some(now) => reset_identity_date(author, now),
        None => author,
    };
    let committer = match (opts.committer_date_is_author_date, now.as_deref()) {
        (_, Some(now)) => {
            reset_identity_date(commit_identity_from_env("COMMITTER", config)?, now)
        }
        (true, None) => {
            let author_date = identity_date(&author).unwrap_or_else(rebase_now_date);
            commit_identity_from_env_with_date("COMMITTER", &author_date, config)?
        }
        (false, None) => commit_identity_from_env("COMMITTER", config)?,
    };
    Ok((author, committer))
}

/// The current wall-clock time as git's raw `@<seconds> +0000`. Mirrors git's
/// `reset_ident_date()` + `datestamp()` path used by `--ignore-date` /
/// `--committer-date-is-author-date`: the upstream tests run under `TZ=UTC`, so
/// the synthesized "now" carries a `+0000` offset.
fn rebase_now_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{secs} +0000")
}

/// Extract the `@<seconds> <tz>` date portion from a raw identity line
/// (`Name <email> <seconds> <tz>`), suitable for `commit_identity_from_env_with_date`.
fn identity_date(identity: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(identity).ok()?;
    let close = text.rfind('>')?;
    let rest = text[close + 1..].trim();
    let mut parts = rest.split_whitespace();
    let seconds = parts.next()?;
    let tz = parts.next()?;
    Some(format!("@{seconds} {tz}"))
}

/// Replace the date portion of a raw identity line (`Name <email> <seconds> <tz>`)
/// with `new_date` (any form `canonicalize_commit_date` accepts), keeping the
/// name+email. Used to reset author/committer dates to "now".
fn reset_identity_date(identity: Vec<u8>, new_date: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(&identity);
    let Some(close) = text.rfind('>') else {
        return identity;
    };
    let canonical = canonicalize_commit_date(new_date);
    let mut out = text[..=close].as_bytes().to_vec();
    out.push(b' ');
    out.extend_from_slice(canonical.as_bytes());
    out
}

fn read_author_script_identity(ctx: &RebaseContext) -> Result<Option<Vec<u8>>> {
    let Ok(text) = fs::read(ctx.state_path("author-script")) else {
        return Ok(None);
    };
    let Some((name, email, date)) = sheet::parse_author_script_bytes(&text) else {
        return Ok(None);
    };
    let identity = crate::format_commit_identity_bytes(&name, &email, &date)?;
    Ok(Some(identity))
}

// ---------------------------------------------------------------------------
// Finishing
// ---------------------------------------------------------------------------

fn finish_rebase(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    opts: &MachineOpts,
) -> Result<()> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let head_name_display;
    if let Some(head_name) = &opts.head_name {
        let committer = committer_identity_for_reflog(&ctx.config)?;
        let mut tx = refs.transaction();
        let branch_already_at_head = matches!(refs.read_ref(head_name)?, Some(RefTarget::Direct(current)) if current == head);
        if !branch_already_at_head {
            tx.update(RefUpdate {
                name: head_name.clone(),
                expected: None,
                new: RefTarget::Direct(head),
                reflog: Some(ReflogEntry {
                    old_oid: opts.orig_head,
                    new_oid: head,
                    committer: committer.clone(),
                    message: ctx.reflog("finish", Some(&format!("{head_name} onto {}", opts.onto))),
                }),
            });
        }
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic(head_name.clone()),
            reflog: Some(ReflogEntry {
                old_oid: head,
                new_oid: head,
                committer,
                message: ctx.reflog("finish", Some(&format!("returning to {head_name}"))),
            }),
        });
        tx.commit()?;
        head_name_display = head_name.clone();
    } else {
        head_name_display = "detached HEAD".to_string();
    }

    if opts.verbose {
        let db = ctx.db();
        let old_tree = commit_tree_oid(&db, ctx.format, &opts.orig_head)?;
        let new_tree = commit_tree_oid(&db, ctx.format, &head)?;
        // Finish (orig-head..HEAD) diffstat: DIFFSTAT only, no summary lines.
        (hosts.print_diffstat)(&old_tree, &new_tree)?;
    }

    run_post_rewrite_hook(ctx, hosts)?;

    apply_autostash(ctx, hosts);
    cleanup_rewritten_refs(ctx);

    if !opts.quiet {
        eprintln!("Successfully rebased and updated {head_name_display}.");
    }

    let update_refs_result = do_update_refs(ctx, opts.quiet);

    sheet::remove_merge_state(&ctx.git_dir);
    update_refs_result
}

pub(crate) fn rewritten_list_path(ctx: &RebaseContext) -> PathBuf {
    ctx.state_path("rewritten-list")
}

pub(crate) fn rewritten_pending_path(ctx: &RebaseContext) -> PathBuf {
    ctx.state_path("rewritten-pending")
}

pub(crate) fn flush_rewritten_pending(ctx: &RebaseContext) -> Result<()> {
    let pending_path = rewritten_pending_path(ctx);
    let pending = fs::read_to_string(&pending_path).unwrap_or_default();
    if pending.is_empty() {
        return Ok(());
    }
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let mut list = fs::read_to_string(rewritten_list_path(ctx)).unwrap_or_default();
    for line in pending.lines().filter(|line| !line.trim().is_empty()) {
        list.push_str(line.trim());
        list.push(' ');
        list.push_str(&head.to_hex());
        list.push('\n');
    }
    write_state_atomic(rewritten_list_path(ctx), list)?;
    let _ = fs::remove_file(pending_path);
    Ok(())
}

pub(crate) fn record_rewritten(
    ctx: &RebaseContext,
    old_oid: &ObjectId,
    next_command: Option<TodoCommand>,
) -> Result<()> {
    let pending_path = rewritten_pending_path(ctx);
    let mut pending = fs::read_to_string(&pending_path).unwrap_or_default();
    pending.push_str(&old_oid.to_hex());
    pending.push('\n');
    write_state_atomic(&pending_path, pending)?;
    if !next_command.is_some_and(TodoCommand::is_fixup) {
        flush_rewritten_pending(ctx)?;
    }
    Ok(())
}

/// Post-rewrite bookkeeping: flush the pending pair list, hand the pairs to
/// the notes-rewrite host service (best-effort), then feed the raw
/// `rewritten-list` bytes to the `post-rewrite` hook (best-effort).
fn run_post_rewrite_hook(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) -> Result<()> {
    flush_rewritten_pending(ctx)?;
    let path = rewritten_list_path(ctx);
    let input = fs::read(&path).unwrap_or_default();
    if input.is_empty() {
        return Ok(());
    }
    // Copy notes from each rewritten commit to its replacement, per
    // `notes.rewrite.rebase` / `notes.rewriteRef` (git does this internally,
    // independent of the post-rewrite hook). Best-effort: a failure here must
    // not fail the (already-finished) rebase.
    let pairs = parse_rewritten_list(ctx, &input);
    if let Err(err) = (hosts.copy_notes_for_rewrite)(&pairs) {
        eprintln!("warning: failed to copy notes: {err}");
    }
    let _ = (hosts.run_hook)("post-rewrite", vec!["rebase".to_string()], Some(input));
    Ok(())
}

/// Parse the `rewritten-list` (one `<old-sha> <new-sha>` pair per line) into
/// resolved object id pairs, skipping any malformed line.
fn parse_rewritten_list(ctx: &RebaseContext, input: &[u8]) -> Vec<(ObjectId, ObjectId)> {
    let text = String::from_utf8_lossy(input);
    let mut pairs = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let (Ok(old), Ok(new)) = (
            ObjectId::from_hex(ctx.format, old),
            ObjectId::from_hex(ctx.format, new),
        ) {
            pairs.push((old, new));
        }
    }
    pairs
}

pub fn finish_rebase_cleanup(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) {
    let _ = fs::remove_file(ctx.git_dir.join("REBASE_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    apply_autostash(ctx, hosts);
    cleanup_rewritten_refs(ctx);
    sheet::remove_merge_state(&ctx.git_dir);
}

pub fn cleanup_rewritten_refs(ctx: &RebaseContext) {
    let refs = ctx.refs();
    let Ok(all_refs) = refs.list_refs() else {
        return;
    };
    for reference in all_refs {
        if reference.name.starts_with("refs/rewritten/") {
            let _ = refs.delete_ref(&reference.name);
        }
    }
}

// ---------------------------------------------------------------------------
// --continue / --skip / --abort / --quit / --edit-todo
// ---------------------------------------------------------------------------

pub fn rebase_continue(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) -> Result<()> {
    let db = ctx.db();
    let opts = sheet::read_rebase_state(&ctx.git_dir, ctx.format)?;

    // Unstaged changes gate.
    let status = (hosts.short_status)()?;
    let unmerged = status.iter().any(|entry| {
        matches!(entry.index, b'U' | b'A' | b'D') && matches!(entry.worktree, b'U' | b'A' | b'D')
    });
    let has_unstaged = status.iter().any(|entry| {
        entry.worktree != b' ' && entry.worktree != b'?' && !is_submodule_only_status(entry)
    });
    if unmerged || has_unstaged {
        println!("You must edit all merge conflicts and then");
        println!("mark them as resolved using git add");
        return Err(GitError::Exit(1));
    }

    let mut todo = read_populate_todo(ctx, &db)?;
    filter_update_refs(ctx, &todo.items)?;
    if ctx.state_path("dropped").exists() {
        if check_todo_dropped_commits_against_backup(ctx, hosts, &todo.items)? {
            return Err(GitError::Exit(1));
        }
        let _ = fs::remove_file(ctx.state_path("dropped"));
    }

    if commit_staged_changes(ctx, hosts, &db, &opts, &todo)? {
        return Err(GitError::Exit(1));
    }

    record_stopped_sha_rewritten(ctx, &todo)?;
    let _ = fs::remove_file(ctx.state_path("stopped-sha"));

    pick_commits(ctx, hosts, &opts, &mut todo)
}

fn record_stopped_sha_rewritten(ctx: &RebaseContext, todo: &TodoList) -> Result<()> {
    if let Ok(raw) = fs::read_to_string(ctx.state_path("stopped-sha"))
        && let Ok(stopped) = ObjectId::from_hex(ctx.format, raw.trim())
    {
        record_rewritten(ctx, &stopped, first_todo_command(todo))?;
    }
    Ok(())
}

/// Returns `true` when the continue must abort (error already printed).
fn commit_staged_changes(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    db: &FileObjectDatabase,
    opts: &MachineOpts,
    todo: &TodoList,
) -> Result<bool> {
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    let head_tree = commit_tree_oid(db, ctx.format, &head)?;
    let index_tree = sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)?;
    let is_clean = index_tree == head_tree;

    let message_path = ctx.state_path("message");
    if !is_clean && !message_path.exists() {
        eprintln!(
            "error: you have staged changes in your working tree\nIf these changes are meant to be squashed into the previous commit, run:\n\n  git commit --amend \n\nIf they are meant to go into a new commit, run:\n\n  git commit \n\nIn both cases, once you're done, continue with:\n\n  git rebase --continue\n"
        );
        return Ok(true);
    }

    let amend_path = ctx.state_path("amend");
    let mut amend = false;
    let mut final_fixup = false;
    let mut edit = true;
    let mut cleanup_only = false;
    if amend_path.exists() {
        let raw = fs::read_to_string(&amend_path)?;
        let to_amend = ObjectId::from_hex(ctx.format, raw.trim())
            .map_err(|_| GitError::InvalidObject("invalid contents: amend".into()))?;
        if !is_clean && head != to_amend {
            eprintln!(
                "error: \nYou have uncommitted changes in your working tree. Please, commit them\nfirst and then run 'git rebase --continue' again."
            );
            return Ok(true);
        }
        let fixup_count = current_fixup_count(ctx);
        if is_clean && fixup_count > 0 {
            if head != to_amend || !ctx.state_path("stopped-sha").exists() {
                // A final fixup/squash was completed manually.
                if !next_is_fixup_first(todo) {
                    let _ = fs::remove_file(ctx.state_path("message-fixup"));
                    let _ = fs::remove_file(ctx.state_path("message-squash"));
                    let _ = fs::remove_file(ctx.state_path("current-fixups"));
                }
            } else {
                // Skipping a failed fixup/squash in a chain.
                let fixups = fs::read_to_string(ctx.state_path("current-fixups"))?;
                let mut lines: Vec<&str> = fixups.lines().collect();
                lines.pop();
                let had_squash = lines.iter().any(|line| line.starts_with("squash "));
                write_state_atomic(ctx.state_path("current-fixups"), lines.join("\n"))?;
                if !lines.is_empty() && !next_is_fixup_first(todo) {
                    final_fixup = true;
                    if !had_squash {
                        edit = false;
                        cleanup_only = true;
                        let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
                        write_state_atomic(
                            ctx.state_path("message-squash"),
                            &head_record.commit.message,
                        )?;
                    } else if let Ok(message_squash) = fs::read(ctx.state_path("message-squash")) {
                        let remaining_messages = lines.len() + 1;
                        let pruned = remove_last_squash_message_section(
                            &message_squash,
                            remaining_messages,
                            &commit_comment_string(&ctx.git_dir),
                        );
                        write_state_atomic(ctx.state_path("message-squash"), pruned)?;
                    }
                } else if next_is_fixup_first(todo) {
                    // Update the squash message to skip the latest commit
                    // message.
                    let head_record = read_rev_list_commit_record(db, ctx.format, head)?;
                    write_state_atomic(
                        ctx.state_path("message-squash"),
                        &head_record.commit.message,
                    )?;
                }
            }
        }
        amend = true;
    }

    if is_clean {
        let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
        let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
        if !final_fixup {
            let _ = fs::remove_file(amend_path);
            return Ok(false);
        }
    }

    let message_file = if final_fixup {
        Some(ctx.state_path("message-squash"))
    } else if amend && is_clean {
        None
    } else {
        Some(message_path)
    };
    let result = machine_commit(
        ctx,
        hosts,
        opts,
        MachineCommit {
            amend,
            edit: edit && !cleanup_only,
            cleanup_message: true,
            allow_empty: true,
            create_root: false,
            message_file,
            reflog_sub: "continue",
            original: None,
        },
    )?;
    if matches!(result, CommitOutcome::Failed(_)) {
        eprintln!("error: could not commit staged changes.");
        return Ok(true);
    }

    let _ = fs::remove_file(ctx.state_path("amend"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    if final_fixup {
        let _ = fs::remove_file(ctx.state_path("message-fixup"));
        let _ = fs::remove_file(ctx.state_path("message-squash"));
    }
    if current_fixup_count(ctx) > 0 {
        let _ = fs::remove_file(ctx.state_path("current-fixups"));
    }
    Ok(false)
}

fn next_is_fixup_first(todo: &TodoList) -> bool {
    todo.items
        .iter()
        .find(|item| item.command != TodoCommand::Comment)
        .is_some_and(|item| item.command.is_fixup())
}

pub fn rebase_skip(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) -> Result<()> {
    let db = ctx.db();
    let opts = sheet::read_rebase_state(&ctx.git_dir, ctx.format)?;
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    reset_index_and_worktree_to_commit_for_rebase(ctx, hosts, &head)?;
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));

    let mut todo = read_populate_todo(ctx, &db)?;
    if commit_staged_changes(ctx, hosts, &db, &opts, &todo)? {
        return Err(GitError::Exit(1));
    }
    record_stopped_sha_rewritten(ctx, &todo)?;
    let _ = fs::remove_file(ctx.state_path("stopped-sha"));
    pick_commits(ctx, hosts, &opts, &mut todo)
}

pub fn rebase_abort(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) -> Result<()> {
    let opts = sheet::read_rebase_state(&ctx.git_dir, ctx.format)?;
    let target = peel_to_commit(&ctx.db(), ctx.format, &opts.orig_head)?;
    reset_index_and_worktree_to_commit_for_rebase(ctx, hosts, &target)?;
    let refs = ctx.refs();
    let committer = committer_identity_for_reflog(&ctx.config)?;
    let old_head = head_commit_oid(refs)?.unwrap_or(ObjectId::null(ctx.format));
    let returning_to = match &opts.head_name {
        Some(head_name) => head_name.clone(),
        None => opts.orig_head.to_hex(),
    };
    let reflog_message = ctx.reflog("abort", Some(&format!("returning to {returning_to}")));
    let mut tx = refs.transaction();
    match &opts.head_name {
        Some(head_name) => {
            // The merge backend ran on a DETACHED HEAD (the pick loop detaches
            // onto `onto`), so the branch ref never moved off orig_head — abort
            // only re-attaches HEAD to it. Updating the branch ref here would
            // add a spurious branch-reflog entry; git leaves it untouched
            // (t3406 #15).
            if refs.read_ref(head_name)?.is_none() {
                tx.update(RefUpdate {
                    name: head_name.clone(),
                    expected: None,
                    new: RefTarget::Direct(target),
                    reflog: None,
                });
            }
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(head_name.clone()),
                reflog: Some(ReflogEntry {
                    old_oid: old_head,
                    new_oid: target,
                    committer,
                    message: reflog_message,
                }),
            });
        }
        None => {
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Direct(target),
                reflog: Some(ReflogEntry {
                    old_oid: old_head,
                    new_oid: target,
                    committer,
                    message: reflog_message,
                }),
            });
        }
    }
    tx.commit()?;
    let _ = fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(ctx.git_dir.join("AUTO_MERGE"));
    finish_rebase_cleanup(ctx, hosts);
    Ok(())
}

pub fn rebase_quit(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) -> Result<()> {
    save_autostash(ctx, hosts);
    cleanup_rewritten_refs(ctx);
    sheet::remove_merge_state(&ctx.git_dir);
    let _ = fs::remove_file(ctx.git_dir.join("REBASE_HEAD"));
    Ok(())
}

pub fn rebase_edit_todo(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) -> Result<()> {
    let db = ctx.db();
    let todo_path = ctx.state_path("git-rebase-todo");
    let text = fs::read_to_string(&todo_path)?;
    let stripped = stripspace_drop_comments(&text, comment_char(&ctx.git_dir));
    let mut resolver = make_resolver(ctx, &db);
    let (items, old_messages) = sheet::parse_todo_buffer(
        &stripped,
        ctx.state_path("done").exists(),
        comment_char(&ctx.git_dir) as char,
        &mut resolver,
    );
    drop(resolver);
    let incorrect = !old_messages.is_empty() || ctx.state_path("dropped").exists();
    write_todo_file(ctx, hosts, &todo_path, &items, true, true, None, None, &db)?;
    if !incorrect {
        write_todo_file(
            ctx,
            hosts,
            &ctx.state_path("git-rebase-todo.backup"),
            &items,
            false,
            true,
            None,
            None,
            &db,
        )?;
    }
    (hosts.launch_sequence_editor)(&todo_path)?;
    let edited = fs::read_to_string(&todo_path)?;
    let stripped = stripspace_drop_comments(&edited, comment_char(&ctx.git_dir));
    let mut resolver = make_resolver(ctx, &db);
    let (new_items, messages) = sheet::parse_todo_buffer(
        &stripped,
        ctx.state_path("done").exists(),
        comment_char(&ctx.git_dir) as char,
        &mut resolver,
    );
    drop(resolver);
    if !messages.is_empty() {
        for message in messages {
            eprintln!("{message}");
        }
        print_edit_todo_recovery_advice();
        return Err(GitError::Exit(1));
    }
    if incorrect {
        for message in old_messages {
            eprintln!("{message}");
        }
        if check_todo_dropped_commits_against_backup(ctx, hosts, &new_items)? {
            return Err(GitError::Exit(1));
        }
        let _ = fs::remove_file(ctx.state_path("dropped"));
    } else if check_todo_dropped_commits(ctx, hosts, &items, &new_items)? {
        return Err(GitError::Exit(1));
    }
    // Reconcile the update-refs state with the edited todo (drop removed
    // update-ref lines, add new ones).
    filter_update_refs(ctx, &new_items)?;
    write_todo_file(ctx, hosts, &todo_path, &new_items, false, false, None, None, &db)?;
    let done_nr = fs::read_to_string(ctx.state_path("done"))
        .map(|text| {
            let mut resolver = make_resolver(ctx, &db);
            let (done_items, _) = sheet::parse_todo_buffer(
                &text,
                true,
                comment_char(&ctx.git_dir) as char,
                &mut resolver,
            );
            sheet::count_commands(&done_items)
        })
        .unwrap_or(0);
    let total_nr = done_nr + sheet::count_commands(&new_items);
    write_state_atomic(ctx.state_path("end"), format!("{total_nr}\n"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Autostash integration
// ---------------------------------------------------------------------------

/// Create the autostash before the rebase starts (`rebase.autostash` /
/// `--autostash`): stash the dirty state into the active backend's state dir
/// and reset back to a clean HEAD.
pub fn create_autostash(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    use_apply_backend: bool,
) -> Result<()> {
    let status = (hosts.short_status)()?;
    let dirty = status.iter().any(|entry| {
        !rebase_status_is_submodule(entry)
            && entry.index != b'?'
            && (entry.index != b' ' || entry.worktree != b' ')
    });
    if !dirty {
        return Ok(());
    }
    let created = (hosts.stash_create)()?;
    let Some(oid) = created else {
        eprintln!("fatal: Cannot autostash");
        return Err(GitError::Exit(128));
    };
    // git records the autostash inside the active backend's state dir
    // (`rebase-apply/` for the apply backend, `rebase-merge/` for the merge
    // sequencer). The t-suite asserts on `$dotest/autostash` per backend, and
    // routing it to the wrong dir both fails those asserts and leaves the other
    // dir behind for the next rebase to trip over.
    let dir = if use_apply_backend {
        ctx.git_dir.join("rebase-apply")
    } else {
        sheet::merge_dir(&ctx.git_dir)
    };
    fs::create_dir_all(&dir)?;
    write_state_atomic(dir.join("autostash"), oid.to_hex())?;
    println!(
        "Created autostash: {}",
        find_unique_abbrev_hex(hosts, &ctx.db(), &oid)
    );
    let refs = ctx.refs();
    let head =
        head_commit_oid(refs)?.ok_or_else(|| GitError::Command("cannot read HEAD".into()))?;
    reset_index_and_worktree_to_commit_for_rebase(ctx, hosts, &head)?;
    Ok(())
}

pub fn apply_autostash(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) {
    apply_save_autostash(ctx, hosts, true);
}

pub fn save_autostash(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) {
    apply_save_autostash(ctx, hosts, false);
}

/// Read the apply backend's autostash marker without consuming it (the caller
/// decides whether to restore or keep it across `--abort`/`--quit`).
pub fn read_apply_autostash(git_dir: &Path) -> Option<String> {
    let path = git_dir.join("rebase-apply").join("autostash");
    fs::read_to_string(path).ok()
}

fn apply_save_autostash(ctx: &RebaseContext, hosts: &RebaseHosts<'_>, attempt_apply: bool) {
    // The autostash lives in whichever backend's state dir created it:
    // `rebase-merge/autostash` (merge sequencer) or `rebase-apply/autostash`
    // (apply backend). Check both so the restore path is backend-agnostic.
    let merge_path = ctx.state_path("autostash");
    let apply_path = ctx.git_dir.join("rebase-apply").join("autostash");
    let path = if merge_path.exists() {
        merge_path
    } else {
        apply_path
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let _ = fs::remove_file(&path);
    apply_save_autostash_text(ctx, hosts, &text, attempt_apply);
}

pub fn apply_save_autostash_text(
    ctx: &RebaseContext,
    hosts: &RebaseHosts<'_>,
    text: &str,
    attempt_apply: bool,
) {
    let oid_text = text.trim().to_string();
    if oid_text.is_empty() {
        return;
    }
    let Ok(oid) = ObjectId::from_hex(ctx.format, &oid_text) else {
        return;
    };
    let applied = attempt_apply && (hosts.stash_apply_quietly)(&oid).unwrap_or(false);
    if applied {
        eprintln!("Applied autostash.");
        return;
    }
    // Store the stash for later.
    let stored = (hosts.stash_store)(&oid, "autostash").is_ok();
    if !stored {
        eprintln!("error: cannot store {oid_text}");
    } else if attempt_apply {
        print_autostash_conflict_advice();
    } else {
        eprintln!("Autostash exists; creating a new stash entry.");
        eprintln!("Your changes are safe in the stash.");
        eprintln!("You can run \"git stash pop\" or \"git stash drop\" at any time.");
    }
}

fn print_autostash_conflict_advice() {
    eprintln!("Your local changes are stashed, however applying them");
    eprintln!("resulted in conflicts.  You can either resolve the conflicts");
    eprintln!("and then discard the stash with \"git stash drop\", or, if you");
    eprintln!("do not want to resolve them now, run \"git reset --hard\" and");
    eprintln!("apply the local changes later by running \"git stash pop\".");
}

/// Restore an autostash for a completed apply-backend rebase and clean up the
/// stray `rebase-merge/` directory `create_autostash` writes the autostash into
/// (the apply backend otherwise only removes `rebase-apply/`, leaving an empty
/// `rebase-merge/` that the next rebase mistakes for an interrupted one).
pub fn finish_apply_autostash(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) {
    apply_autostash(ctx, hosts);
    sheet::remove_merge_state(&ctx.git_dir);
}

pub fn cleanup_autostash_and_state(ctx: &RebaseContext, hosts: &RebaseHosts<'_>) {
    apply_autostash(ctx, hosts);
    sheet::remove_merge_state(&ctx.git_dir);
}

#[cfg(test)]
mod native_strategy_tests {
    use super::{custom_rebase_strategy_needs_external_driver, is_unimplemented_git_core_merge_strategy};

    #[test]
    fn git_core_merge_helpers_never_fall_through_to_path() {
        for strategy in ["octopus", "one-file", "ours", "subtree"] {
            assert!(custom_rebase_strategy_needs_external_driver(strategy));
            assert!(is_unimplemented_git_core_merge_strategy(strategy));
        }
        for strategy in ["ort", "recursive", "resolve"] {
            assert!(!custom_rebase_strategy_needs_external_driver(strategy));
            assert!(!is_unimplemented_git_core_merge_strategy(strategy));
        }
        assert!(custom_rebase_strategy_needs_external_driver(
            "custom-driver"
        ));
        assert!(!is_unimplemented_git_core_merge_strategy("custom-driver"));
    }
}

#[cfg(test)]
mod atomic_state_write_tests {
    use super::write_state_atomic;
    use std::fs;
    use std::path::PathBuf;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sley-rebase-drive-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// The kill-mid-write property: a truncated temp left by a crashed writer
    /// (simulated here as partial content at the sibling `.lock` path) must
    /// never leak into the final path. Publication fails cleanly, the previous
    /// full contents survive, and a later write publishes atomically.
    #[test]
    fn truncated_tmp_never_visible_at_final_path() {
        let dir = scratch_dir("atomic");
        let path = dir.join("git-rebase-todo");
        fs::write(&path, b"pick aaa1111\n").expect("seed target");
        let lock = dir.join("git-rebase-todo.lock");
        fs::write(&lock, b"pick aaa11").expect("simulate crashed temp");

        let err = write_state_atomic(&path, b"pick bbb2222\n").expect_err("held lock must fail");
        assert_eq!(err.io_kind(), Some(std::io::ErrorKind::AlreadyExists));
        assert_eq!(
            fs::read(&path).expect("read target"),
            b"pick aaa1111\n",
            "final path must keep the previous full contents"
        );

        // Once the stale temp is gone, publication succeeds and cleans up.
        fs::remove_file(&lock).expect("remove stale lock");
        write_state_atomic(&path, b"pick bbb2222\n").expect("atomic publish");
        assert_eq!(fs::read(&path).expect("read target"), b"pick bbb2222\n");
        assert!(!lock.exists(), "publication must consume its temp");

        fs::remove_dir_all(dir).ok();
    }
}





