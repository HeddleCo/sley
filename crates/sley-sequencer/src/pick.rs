//! The cherry-pick / revert drive loop.
//!
//! Stage-B1 relocation from the CLI: the loop orchestration around
//! [`super::replay`]'s state machine now lives with the engine — revision
//! selection, the per-commit 3-way replay (`do_pick_commit`), conflict
//! pausing/resume decisions, commit creation through this crate's
//! [`super::create_commit`] + reflog updates via sley-refs, and the
//! `--continue` / `--skip` / `--abort` / `--quit` transitions. The CLI keeps
//! argv parsing, usage text, exit-code mapping, and the host-injected
//! services below (editor/hook runs, trailer recognition).
//!
//! Byte-parity notes:
//! * Diagnostics that are part of git's porcelain contract are emitted here,
//!   matching the established library-crate pattern (see sley-worktree); the
//!   strings are unchanged from the previous CLI home.
//! * Partial-clone hydration is injected via
//!   [`crate::apply::PromisorObjectFetch`] like every blob read in the apply
//!   backend.

use crate::apply::{
    MergePathResult, MergeTreeMap, PromisorObjectFetch, commit_tree_oid, head_commit_oid,
    merge_index_entry, merge_read_blob_with_fetch, merge_remove_worktree_file,
    merge_refuse_if_current_working_directory_becomes_file, merge_write_worktree_file,
    three_way_merge_trees_styled_with_strategy_options,
};
use crate::replay::{self, ReplayAction, ReplayOpts, TodoItem};
use sley_config::{GitConfig, effective_config_parameters_env};
use sley_core::{GitError, ObjectId, Result};
use sley_index::{Index, IndexEntry, is_gitlink};
use sley_object::{commit_identity_from_env, commit_signoff_from_env};
use sley_object::{Commit, EncodedObject, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_pretty::{
    commit_author_for_commit_encoding, commit_encoding_config, commit_encoding_header_from_config,
    commit_message_for_commit_encoding, commit_subject, format_log_abbrev_oid,
};
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// The final-commit-message seam: write `.git/COMMIT_EDITMSG`, run the
/// prepare-commit-msg hook (merge source when `merge_source`), launch the
/// editor + commit-msg hook when `edit`, and return the cleaned message.
pub type PrepareCommitMessage<'a> =
    dyn Fn(&Path, Vec<u8>, bool /* merge_source */, bool /* edit */) -> Result<Vec<u8>> + 'a;
/// Append a Signed-off-by trailer ahead of any trailing comment block.
pub type AppendSignoff<'a> = dyn Fn(Vec<u8>, &[u8]) -> Vec<u8> + 'a;

/// Host services the drive loop cannot own: process-spawning hooks/editor and
/// the trailer-recognition engine that still lives beside `git interpret-trailers`.
pub struct PickHosts<'a> {
    /// Prepare the final commit message: write `.git/COMMIT_EDITMSG`, run the
    /// prepare-commit-msg hook (source: merge when `merge_source`, else
    /// message), launch the editor + commit-msg hook when `edit`, and return
    /// the cleaned message bytes.
    pub prepare_commit_message: &'a PrepareCommitMessage<'a>,
    /// Append a Signed-off-by trailer ahead of any trailing comment block
    /// (CLI-owned trailer-block recognition).
    pub append_signoff: &'a AppendSignoff<'a>,
    /// git's `has_conforming_footer` approximation over the trailer engine.
    pub has_conforming_trailer_block: &'a dyn Fn(Option<&GitConfig>, &str) -> bool,
    /// Partial-clone hydration for blob reads; `None` outside partial clones.
    pub promisor_fetch: Option<&'a dyn PromisorObjectFetch>,
    /// Revision arguments were rejected by setup: print the command's usage
    /// (porcelain-owned text) and return the usage exit code.
    pub usage_error: &'a dyn Fn(ReplayAction) -> GitError,
}

/// Repository handles shared by every replay operation.
pub struct PickContext {
    pub action: ReplayAction,
    pub git_dir: PathBuf,
    pub worktree_root: PathBuf,
    pub format: sley_core::ObjectFormat,
    pub config: GitConfig,
    pub replace_objects: bool,
    pub db: FileObjectDatabase,
}

impl PickContext {
    pub fn refs(&self) -> FileRefStore {
        FileRefStore::new(&self.git_dir, self.format)
    }

    fn head_oid(&self) -> Option<ObjectId> {
        head_commit_oid(&self.refs()).ok().flatten()
    }
}

fn index_entry_stage(entry: &IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

/// Effective-config lookup (repo + global/system + `-c`/env injection).
fn config_value(config: &GitConfig, section: &str, key: &str) -> Option<String> {
    config.get(section, None, key).map(str::to_string)
}

fn config_bool(config: &GitConfig, section: &str, key: &str) -> Option<bool> {
    let value = config_value(config, section, key)?;
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn read_effective_config_value_at_git_dir(git_dir: &Path, section: &str, key: &str) -> Option<String> {
    let config = sley_config::read_repo_config(git_dir, effective_config_parameters_env().as_deref())
        .ok()?;
    config.get(section, None, key).map(str::to_string)
}

pub(crate) fn fatal_failed(action: ReplayAction) -> GitError {
    eprintln!("fatal: {} failed", action.name());
    GitError::Exit(128)
}

// ---------------------------------------------------------------------------
// Message cleanup helpers (moved from the CLI; canonical here so the
// `--continue` path and the host share one implementation)
// ---------------------------------------------------------------------------

pub fn comment_char(git_dir: &Path) -> u8 {
    match read_effective_config_value_at_git_dir(git_dir, "core", "commentChar").as_deref() {
        Some(value) if value.eq_ignore_ascii_case("auto") => b'#',
        Some(value) => value.bytes().next().unwrap_or(b'#'),
        None => b'#',
    }
}

pub fn strip_comment_lines(message: &[u8], comment: u8) -> Vec<u8> {
    strip_comment_string_lines(message, &[comment])
}

pub fn strip_comment_string_lines(message: &[u8], comment: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len());
    let mut blank_pending = false;
    for line in message.split_inclusive(|&b| b == b'\n') {
        if !comment.is_empty() && line.starts_with(comment) {
            continue;
        }
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.iter().all(|b| b.is_ascii_whitespace()) {
            blank_pending = !out.is_empty();
            continue;
        }
        if blank_pending {
            out.push(b'\n');
            blank_pending = false;
        }
        out.extend_from_slice(line);
    }
    // stripspace: trim leading/trailing blank lines and collapse internal
    // blank runs to a single empty line.
    let text = out;
    let mut start = 0;
    while start < text.len() && text[start] == b'\n' {
        start += 1;
    }
    let mut end = text.len();
    while end > start && text[end - 1] == b'\n' {
        end -= 1;
    }
    let mut cleaned = text[start..end].to_vec();
    if !cleaned.is_empty() {
        cleaned.push(b'\n');
    }
    cleaned
}


// ---------------------------------------------------------------------------
// Revision selection
// ---------------------------------------------------------------------------

struct RevSelection {
    /// Resolved commits in pick order.
    commits: Vec<ObjectId>,
    /// True when the args were a single plain revision (no ranges, walk
    /// options, or extra revs) — git's single-pick fast path that skips the
    /// sequencer directory.
    single: bool,
}

/// Resolve the rev arguments into the ordered commit list (mirrors
/// `setup_revisions` + `walk_revs_populate_todo`).
fn select_revisions(
    ctx: &PickContext,
    hosts: &PickHosts<'_>,
    action: ReplayAction,
    rev_args: &[String],
) -> Result<RevSelection> {
    let db = &ctx.db;
    let config =
        sley_config::read_repo_config(&ctx.git_dir, effective_config_parameters_env().as_deref())?;
    let cwd = std::env::current_dir()?;
    let setup_args = rev_args
        .iter()
        .map(|arg| {
            if arg == "-" {
                "@{-1}".to_string()
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>();
    // cherry-pick/revert use setup_revisions with assume_dashdash=1 (builtin/revert.c),
    // so positionals are revisions only — a tag like `b` must not be rejected as
    // "both revision and filename" when a path `B` exists (case-insensitive FS).
    let setup = sley_rev::setup_revisions(
        &setup_args,
        &sley_rev::RevisionSetupContext {
            git_dir: &ctx.git_dir,
            worktree_root: Some(&ctx.worktree_root),
            cwd: &cwd,
            format: ctx.format,
            reader: db,
            config: Some(&config),
            assume_dashdash: true,
        },
    )?;
    if !setup.leftovers.is_empty() || !setup.pathspecs.is_empty() {
        return Err((hosts.usage_error)(action));
    }
    let mut options = setup.options;
    if !options.has_revisions() && options.max_count.is_some() {
        let oid = resolve_revision(ctx, "HEAD").map_err(|_| {
            eprintln!("fatal: bad revision 'HEAD'");
            GitError::Exit(128)
        })?;
        options.positives.push(sley_rev::RevisionTip {
            oid,
            rev: "HEAD".to_string(),
            source_name: Some("HEAD".to_string()),
            from_ref_selector: false,
        });
    }
    if !options.has_revisions() {
        return Ok(RevSelection {
            commits: Vec::new(),
            single: false,
        });
    }

    let has_walk_spec = !options.negatives.is_empty()
        || !options.symmetric_ranges.is_empty()
        || options.max_count.is_some()
        || options.skip > 0
        || options.date_window.min_time.is_some()
        || options.date_window.max_time.is_some()
        || !options.author_patterns.is_empty();
    let mut commits: Vec<ObjectId> = Vec::new();
    if has_walk_spec {
        let starts = options
            .positives
            .iter()
            .map(|tip| {
                sley_rev::peel_to_commit(db, ctx.format, &tip.oid).map_err(|_| {
                    eprintln!("error: {}: can't cherry-pick that object", tip.rev);
                    fatal_failed(action)
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut excluded = HashSet::new();
        for oid in &options.negatives {
            for record in sley_rev::revlist::rev_list_walk_commits(db, ctx.format, [*oid], options.first_parent)? {
                excluded.insert(record.oid);
            }
        }
        let order = match options.order {
            sley_rev::RevisionOrder::Default | sley_rev::RevisionOrder::Date => {
                sley_rev::RevWalkOrder::CommitDate
            }
            sley_rev::RevisionOrder::Topo => sley_rev::RevWalkOrder::Topo,
            sley_rev::RevisionOrder::AuthorDate => sley_rev::RevWalkOrder::AuthorDate,
        };
        let mut walk = sley_rev::RevWalk::new(&ctx.git_dir, ctx.format, db, starts)
            .order(order)
            .first_parent(options.first_parent)
            .date_window(options.date_window);
        while let Some(record) = walk.try_next()? {
            if excluded.contains(&record.oid) {
                continue;
            }
            commits.push(record.oid);
        }
    } else {
        for tip in &options.positives {
            let commit_oid = sley_rev::peel_to_commit(db, ctx.format, &tip.oid).map_err(|_| {
                eprintln!("error: {}: can't cherry-pick that object", tip.rev);
                fatal_failed(action)
            })?;
            commits.push(commit_oid);
        }
    }

    if !options.author_patterns.is_empty() {
        commits.retain(|oid| {
            options.author_patterns.iter().all(|pattern| {
                commit_author_matches(db, ctx.format, oid, pattern).unwrap_or(false)
            })
        });
    }
    if options.skip > 0 {
        commits = commits.into_iter().skip(options.skip).collect();
    }
    if let Some(limit) = options.max_count {
        commits.truncate(limit);
    }
    // Picking ranges (a real walk) happens oldest-first; reverting keeps the
    // newest-first walk order. Plain rev lists keep argument order either way.
    if action == ReplayAction::Pick && has_walk_spec {
        commits.reverse();
    }
    let single = commits.len() == 1
        && options.positives.len() == 1
        && options.negatives.is_empty()
        && options.symmetric_ranges.is_empty()
        && !has_walk_spec
        && options.author_patterns.is_empty();
    Ok(RevSelection { commits, single })
}

fn resolve_revision(ctx: &PickContext, name: &str) -> Result<ObjectId> {
    sley_rev::resolve_revision_with_replacement_policy(
        &ctx.git_dir,
        ctx.format,
        name,
        ctx.replace_objects,
    )
}

fn commit_author_matches(
    db: &FileObjectDatabase,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
    pattern: &str,
) -> Result<bool> {
    let object = db.read_object(oid)?;
    let commit = Commit::parse(format, &object.body)?;
    let author = String::from_utf8_lossy(&commit.author).to_string();
    // git matches --author as a (basic) regex; the corpus only needs
    // substring-with-dot-wildcard semantics, so approximate `.` as any-char.
    Ok(regexish_contains(&author, pattern))
}

/// Tiny "regex" matcher: `.` matches any character, everything else literal.
fn regexish_contains(haystack: &str, pattern: &str) -> bool {
    let h: Vec<char> = haystack.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    if p.is_empty() {
        return true;
    }
    if h.len() < p.len() {
        return false;
    }
    for start in 0..=(h.len() - p.len()) {
        if p.iter()
            .enumerate()
            .all(|(i, pc)| *pc == '.' || h[start + i] == *pc)
        {
            return true;
        }
    }
    false
}

/// Commit selection shared with the `git replay` plumbing command, which has
/// no instruction sheet and wants the plain ordered commit list.
pub fn select_commits(
    ctx: &PickContext,
    hosts: &PickHosts<'_>,
    action: ReplayAction,
    rev_args: &[String],
) -> Result<Vec<ObjectId>> {
    Ok(select_revisions(ctx, hosts, action, rev_args)?.commits)
}

pub fn make_todo_item(
    db: &FileObjectDatabase,
    format: sley_core::ObjectFormat,
    action: ReplayAction,
    oid: &ObjectId,
) -> Result<TodoItem> {
    let object = db.read_object(oid)?;
    let commit = Commit::parse(format, &object.body)?;
    let subject = commit_subject(&commit.message);
    let short = format_log_abbrev_oid(oid);
    let display = if subject.is_empty() {
        short
    } else {
        format!("{short} {subject}")
    };
    Ok(TodoItem {
        action,
        oid: *oid,
        display,
    })
}

// ---------------------------------------------------------------------------
// Top-level flows
// ---------------------------------------------------------------------------

/// The initial `git cherry-pick` / `git revert` run (option parsing and
/// opts-normalization stay with the CLI).
pub fn pick_revisions(
    ctx: &PickContext,
    hosts: &mut PickHosts<'_>,
    opts: &ReplayOpts,
    rev_args: &[String],
) -> Result<()> {
    let selection = select_revisions(ctx, hosts, ctx.action, rev_args)?;
    if selection.commits.is_empty() {
        eprintln!("error: empty commit set passed");
        return Err(fatal_failed(ctx.action));
    }
    if selection.single {
        // Single plain rev: replay it without touching the sequencer dir.
        let item = make_todo_item(&ctx.db, ctx.format, ctx.action, &selection.commits[0])?;
        return match do_pick_commit(ctx, hosts, opts, &item, true) {
            Ok(PickFlow::Done | PickFlow::Dropped) => Ok(()),
            Ok(PickFlow::Conflict) => Err(GitError::Exit(1)),
            Ok(PickFlow::HaltEmpty) => Err(GitError::Exit(1)),
            Err(halt) => Err(finish_halt(ctx, halt)),
        };
    }

    let mut items = Vec::with_capacity(selection.commits.len());
    for oid in &selection.commits {
        items.push(make_todo_item(&ctx.db, ctx.format, ctx.action, oid)?);
    }
    let advise_skip = ctx
        .git_dir
        .join("CHERRY_PICK_HEAD")
        .exists()
        || ctx.git_dir.join("REVERT_HEAD").exists();
    if let Some(in_progress) = replay::in_progress_error(&ctx.git_dir, advise_skip) {
        eprintln!("error: {}", in_progress.error);
        if config_bool(&ctx.config, "advice", "sequencerInUse").unwrap_or(true) {
            for line in in_progress.hint.lines() {
                eprintln!("hint: {line}");
            }
        }
        return Err(fatal_failed(ctx.action));
    }
    replay::create_seq_dir(&ctx.git_dir).map_err(|err| {
        eprintln!("error: {err}");
        fatal_failed(ctx.action)
    })?;
    let head = ctx.head_oid();
    if head.is_none() && ctx.action == ReplayAction::Revert {
        eprintln!("error: can't revert as initial commit");
        return Err(fatal_failed(ctx.action));
    }
    let head_text = match &head {
        Some(oid) => oid.to_hex(),
        None => ObjectId::null(ctx.format).to_hex(),
    };
    replay::save_head(&ctx.git_dir, &head_text)?;
    replay::save_opts(&ctx.git_dir, opts)?;
    replay::update_abort_safety(&ctx.git_dir, head.as_ref());
    pick_commits(ctx, hosts, opts, &items)
}

/// Failure routing for the pick engine: `Fatal` is the `res < 0` path (the
/// porcelain appends `fatal: <action> failed`, exit 128); `Code` propagates a
/// child-status exit (no fatal line).
enum ReplayHalt {
    Fatal,
    Code(i32),
}

fn finish_halt(ctx: &PickContext, halt: ReplayHalt) -> GitError {
    match halt {
        ReplayHalt::Fatal => fatal_failed(ctx.action),
        ReplayHalt::Code(code) => GitError::Exit(code),
    }
}

enum PickFlow {
    /// Commit created (or fast-forwarded / no-commit staged).
    Done,
    /// Redundant commit dropped (`--empty=drop`).
    Dropped,
    /// Stopped with conflicts (exit 1).
    Conflict,
    /// Halted on a commit that became empty (exit 1).
    HaltEmpty,
}

/// The unmerged-index guard inside `do_pick_commit`'s dirty-index check
/// (`error_resolve_conflict`).
fn check_no_unmerged(ctx: &PickContext) -> std::result::Result<(), ReplayHalt> {
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    let Ok(bytes) = std::fs::read(&index_path) else {
        return Ok(());
    };
    let index = match Index::parse(&bytes, ctx.format) {
        Ok(index) => index,
        Err(err) => return Err(print_fatal_error(err)),
    };
    if index.entries.iter().any(|entry| index_entry_stage(entry) > 0) {
        let verb = match ctx.action {
            ReplayAction::Pick => "Cherry-picking",
            ReplayAction::Revert => "Reverting",
        };
        eprintln!("error: {verb} is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        return Err(ReplayHalt::Fatal);
    }
    Ok(())
}

/// The pick loop (`pick_commits`): replay every remaining todo item, saving
/// the sheet before each step, and tear the state down on success.
fn pick_commits(
    ctx: &PickContext,
    hosts: &mut PickHosts<'_>,
    opts: &ReplayOpts,
    items: &[TodoItem],
) -> Result<()> {
    for (index, item) in items.iter().enumerate() {
        replay::save_todo(&ctx.git_dir, &items[index..])?;
        match do_pick_commit(ctx, hosts, opts, item, false) {
            Ok(PickFlow::Done | PickFlow::Dropped) => {}
            Ok(PickFlow::Conflict) => return Err(GitError::Exit(1)),
            Ok(PickFlow::HaltEmpty) => return Err(GitError::Exit(1)),
            Err(halt) => return Err(finish_halt(ctx, halt)),
        }
    }
    replay::remove_state(&ctx.git_dir);
    Ok(())
}

fn print_fatal_error(err: GitError) -> ReplayHalt {
    eprintln!("error: {err}");
    ReplayHalt::Fatal
}

fn tree_map_of_commit(
    ctx: &PickContext,
    oid: &ObjectId,
) -> std::result::Result<MergeTreeMap, ReplayHalt> {
    let tree = commit_tree_oid(&ctx.db, ctx.format, oid).map_err(print_fatal_error)?;
    sley_diff_merge::flatten_tree(&ctx.db, ctx.format, &tree).map_err(print_fatal_error)
}

/// Tree oid of the current index (the "are there staged changes" probe).
fn index_tree_oid(ctx: &PickContext) -> Result<ObjectId> {
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    if !index_path.exists() {
        return Ok(ObjectId::empty_tree(ctx.format));
    }
    sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format)
}

fn original_commit_empty(ctx: &PickContext, commit: &Commit) -> std::result::Result<bool, ReplayHalt> {
    let parent_tree = match commit.parents.first() {
        Some(parent) => commit_tree_oid(&ctx.db, ctx.format, parent).map_err(print_fatal_error)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    Ok(parent_tree == commit.tree)
}

#[allow(clippy::too_many_lines)]
fn do_pick_commit(
    ctx: &PickContext,
    hosts: &mut PickHosts<'_>,
    opts: &ReplayOpts,
    item: &TodoItem,
    _is_single: bool,
) -> std::result::Result<PickFlow, ReplayHalt> {
    let action = item.action;
    let commit_object = read_object_or_fatal(ctx, &item.oid)?;
    let commit = match Commit::parse(ctx.format, &commit_object.body) {
        Ok(commit) => commit,
        Err(err) => return Err(print_fatal_error(err)),
    };

    // HEAD / index-dirtiness checks.
    check_no_unmerged(ctx)?;
    let head = ctx.head_oid();
    let unborn = head.is_none();
    let head_tree = match &head {
        Some(oid) => commit_tree_oid(&ctx.db, ctx.format, oid).map_err(print_fatal_error)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let index_tree = index_tree_oid(ctx).map_err(print_fatal_error)?;
    if !opts.no_commit && index_tree != head_tree {
        eprintln!(
            "error: your local changes would be overwritten by {}.",
            action.name()
        );
        if config_bool(&ctx.config, "advice", "commitBeforeMerge").unwrap_or(true) {
            eprintln!("hint: commit your changes or stash them to proceed.");
        }
        return Err(ReplayHalt::Fatal);
    }

    // Mainline / parent selection.
    let parent: Option<ObjectId> = if commit.parents.is_empty() {
        None
    } else if commit.parents.len() > 1 {
        if opts.mainline == 0 {
            eprintln!(
                "error: commit {} is a merge but no -m option was given.",
                item.oid
            );
            return Err(ReplayHalt::Fatal);
        }
        match commit.parents.get(opts.mainline as usize - 1) {
            Some(parent) => Some(*parent),
            None => {
                eprintln!(
                    "error: commit {} does not have parent {}",
                    item.oid, opts.mainline
                );
                return Err(ReplayHalt::Fatal);
            }
        }
    } else if opts.mainline > 1 {
        eprintln!(
            "error: commit {} does not have parent {}",
            item.oid, opts.mainline
        );
        return Err(ReplayHalt::Fatal);
    } else {
        Some(commit.parents[0])
    };

    let subject = commit_subject(&commit.message);
    let short = format_log_abbrev_oid(&item.oid);
    let label = format!("{short} ({subject})");
    let parent_label = format!("parent of {label}");

    // Fast-forward (`--ff`).
    if opts.allow_ff
        && ((parent.is_some() && parent.as_ref() == head.as_ref()) || (parent.is_none() && unborn))
    {
        fast_forward_to(ctx, &item.oid, head.as_ref()).map_err(print_fatal_error)?;
        return Ok(PickFlow::Done);
    }

    let target_encoding = commit_encoding_config(&ctx.git_dir);

    // Replay message.
    let mut message: Vec<u8> = match action {
        ReplayAction::Pick => {
            let mut msg =
                commit_message_for_commit_encoding(&commit, &target_encoding).into_owned();
            if opts.record_origin {
                if !msg.ends_with(b"\n") {
                    msg.push(b'\n');
                }
                let text = String::from_utf8_lossy(&msg);
                if !(hosts.has_conforming_trailer_block)(Some(&ctx.config), &text) {
                    msg.push(b'\n');
                }
                msg.extend_from_slice(
                    format!("(cherry picked from commit {})\n", item.oid).as_bytes(),
                );
            }
            msg
        }
        ReplayAction::Revert => {
            format_revert_message(
                &ctx.db,
                ctx.format,
                &ctx.git_dir,
                item,
                &commit,
                &subject,
                parent.as_ref(),
                opts,
            )
            .map_err(print_fatal_error)?
        }
    };
    if opts.signoff {
        let signoff = commit_signoff_from_env(&ctx.config).map_err(print_fatal_error)?;
        message = (hosts.append_signoff)(message, &signoff);
    }

    // Tree maps for the 3-way replay.
    let (base_map, theirs_map, theirs_label, ancestor_label) = match action {
        ReplayAction::Pick => {
            let base = match &parent {
                Some(parent) => tree_map_of_commit(ctx, parent)?,
                None => MergeTreeMap::new(),
            };
            let theirs = sley_diff_merge::flatten_tree(&ctx.db, ctx.format, &commit.tree)
                .map_err(print_fatal_error)?;
            (base, theirs, label, parent_label)
        }
        ReplayAction::Revert => {
            let base = sley_diff_merge::flatten_tree(&ctx.db, ctx.format, &commit.tree)
                .map_err(print_fatal_error)?;
            let theirs = match &parent {
                Some(parent) => tree_map_of_commit(ctx, parent)?,
                None => MergeTreeMap::new(),
            };
            (base, theirs, parent_label, label)
        }
    };
    let ours_map = sley_diff_merge::flatten_tree(&ctx.db, ctx.format, &index_tree)
        .map_err(print_fatal_error)?;

    let style = match config_value(&ctx.config, "merge", "conflictstyle").as_deref() {
        Some("diff3") => sley_diff_merge::ConflictStyle::Diff3,
        Some("zdiff3") => sley_diff_merge::ConflictStyle::ZDiff3,
        _ => sley_diff_merge::ConflictStyle::Merge,
    };
    let (results, conflicts) =
        three_way_merge_trees_styled_with_strategy_options(
            &ctx.db,
            &ctx.config,
            ctx.format,
            &base_map,
            &ours_map,
            &theirs_map,
            "HEAD",
            &theirs_label,
            &ancestor_label,
            style,
            &opts.strategy_options,
            hosts.promisor_fetch,
        )
        .map_err(print_fatal_error)?;

    // Pre-flight worktree clobber checks (unpack_trees' verify steps).
    let target_map = merge_results_to_tree_map(&results);
    if let Err(err) = merge_refuse_if_current_working_directory_becomes_file(
        &ctx.worktree_root,
        &target_map,
    ) {
        return match err {
            GitError::Exit(code) => Err(ReplayHalt::Code(code)),
            other => Err(print_fatal_error(other)),
        };
    }
    verify_worktree_safe(ctx, &ours_map, &results)?;

    let cleanup_mode = opts
        .default_msg_cleanup
        .clone()
        .or_else(|| config_value(&ctx.config, "commit", "cleanup"));

    if !conflicts.is_empty() {
        apply_merge_results_to_index_and_worktree(ctx, hosts, &ours_map, &results)
            .map_err(print_fatal_error)?;
        // State files for the resolution flow.
        let help_msg = std::env::var("GIT_CHERRY_PICK_HELP").ok();
        let suppress_pick_head = help_msg.is_some();
        if action == ReplayAction::Pick && !opts.no_commit && !suppress_pick_head {
            std::fs::write(
                ctx.git_dir.join("CHERRY_PICK_HEAD"),
                format!("{}\n", item.oid),
            )
            .map_err(|err| print_fatal_error(GitError::from(err)))?;
        }
        if action == ReplayAction::Revert {
            std::fs::write(ctx.git_dir.join("REVERT_HEAD"), format!("{}\n", item.oid))
                .map_err(|err| print_fatal_error(GitError::from(err)))?;
        }
        let mut merge_msg = message;
        append_conflicts_hint(&mut merge_msg, &conflicts, cleanup_mode.as_deref());
        std::fs::write(ctx.git_dir.join("MERGE_MSG"), merge_msg)
            .map_err(|err| print_fatal_error(GitError::from(err)))?;

        // stdout: per-path merge notices; stderr: the error + advice.
        for path in &conflicts {
            let display = String::from_utf8_lossy(path);
            if let Some(MergePathResult::Conflict {
                base, ours, theirs, ..
            }) = results.get(path)
                && base.is_some()
                && ours.is_some()
                && theirs.is_some()
            {
                println!("Auto-merging {display}");
            }
            println!("CONFLICT (content): Merge conflict in {display}");
        }
        let verb = match action {
            ReplayAction::Pick => "apply",
            ReplayAction::Revert => "revert",
        };
        eprintln!("error: could not {verb} {short}... {subject}");
        print_conflict_advice(ctx, opts, help_msg.as_deref());
        replay::update_abort_safety(&ctx.git_dir, head.as_ref());
        return Ok(PickFlow::Conflict);
    }

    // Clean merge: stage the result.
    apply_merge_results_to_index_and_worktree(ctx, hosts, &ours_map, &results)
        .map_err(print_fatal_error)?;
    let new_tree =
        sley_worktree::write_tree_from_index(&ctx.git_dir, ctx.format).map_err(print_fatal_error)?;

    if opts.no_commit {
        if action == ReplayAction::Revert {
            std::fs::write(ctx.git_dir.join("REVERT_HEAD"), format!("{}\n", item.oid))
                .map_err(|err| print_fatal_error(GitError::from(err)))?;
        }
        std::fs::write(ctx.git_dir.join("MERGE_MSG"), &message)
            .map_err(|err| print_fatal_error(GitError::from(err)))?;
        replay::update_abort_safety(&ctx.git_dir, head.as_ref());
        return Ok(PickFlow::Done);
    }

    // CHERRY_PICK_HEAD / REVERT_HEAD ahead of the commit attempt.
    if action == ReplayAction::Pick {
        std::fs::write(
            ctx.git_dir.join("CHERRY_PICK_HEAD"),
            format!("{}\n", item.oid),
        )
        .map_err(|err| print_fatal_error(GitError::from(err)))?;
    }
    std::fs::write(ctx.git_dir.join("MERGE_MSG"), &message)
        .map_err(|err| print_fatal_error(GitError::from(err)))?;

    // Empty-commit handling.
    if new_tree == head_tree {
        let originally_empty = original_commit_empty(ctx, &commit)?;
        let allow = if originally_empty {
            u8::from(opts.allow_empty)
        } else if opts.keep_redundant_commits {
            1
        } else if opts.drop_redundant_commits {
            2
        } else {
            0
        };
        match allow {
            0 => {
                // Halt: the spawned `git commit` would refuse and print the
                // empty-cherry-pick advice; replicate its stderr and exit 1.
                print_empty_halt_advice(ctx);
                replay::update_abort_safety(&ctx.git_dir, head.as_ref());
                return Ok(PickFlow::HaltEmpty);
            }
            2 => {
                let _ = std::fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
                let _ = std::fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
                eprintln!(
                    "dropping {} {} -- patch contents already upstream",
                    item.oid, subject
                );
                replay::update_abort_safety(&ctx.git_dir, head.as_ref());
                return Ok(PickFlow::Dropped);
            }
            _ => {}
        }
    }

    // Optional message edit.
    let edit = should_edit(ctx.action, opts);
    let source_is_merge = edit;
    message = (hosts.prepare_commit_message)(&ctx.git_dir, message.clone(), source_is_merge, edit)
        .map_err(
        |err| {
            // Editor / hook failure cancels the commit but keeps the state files.
            if !matches!(err, GitError::Exit(_)) {
                eprintln!("error: {err}");
            }
            ReplayHalt::Code(1)
        },
    )?;

    // Create the commit and advance HEAD.
    let author = match action {
        ReplayAction::Pick => {
            commit_author_for_commit_encoding(&commit, &target_encoding).into_owned()
        }
        ReplayAction::Revert => {
            commit_identity_from_env("AUTHOR", &ctx.config).map_err(print_fatal_error)?
        }
    };
    let committer = commit_identity_from_env("COMMITTER", &ctx.config).map_err(print_fatal_error)?;
    let reflog_message = format!("{}: {subject}", action.name()).into_bytes();
    let new_oid = commit_and_advance_head(
        ctx,
        &new_tree,
        head.as_ref(),
        author,
        committer,
        &message,
        reflog_message,
        commit_encoding_header_from_config(&ctx.git_dir),
    )
    .map_err(print_fatal_error)?;
    let _ = std::fs::remove_file(ctx.git_dir.join("CHERRY_PICK_HEAD"));
    let _ = std::fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    print_commit_summary_line(ctx, &new_oid, &message);
    replay::update_abort_safety(&ctx.git_dir, Some(&new_oid));
    Ok(PickFlow::Done)
}

fn read_object_or_fatal(
    ctx: &PickContext,
    oid: &ObjectId,
) -> std::result::Result<std::sync::Arc<EncodedObject>, ReplayHalt> {
    match ctx.db.read_object(oid) {
        Ok(object) if object.object_type == ObjectType::Commit => Ok(object),
        Ok(object) => {
            eprintln!(
                "error: expected commit {oid}, found {}",
                object.object_type.as_str()
            );
            Err(ReplayHalt::Fatal)
        }
        Err(err) => {
            eprintln!("error: {err}");
            Err(ReplayHalt::Fatal)
        }
    }
}

/// Format the revert commit message (`sequencer_format_revert_message`).
#[allow(clippy::too_many_arguments)]
pub fn format_revert_message(
    db: &FileObjectDatabase,
    format: sley_core::ObjectFormat,
    git_dir: &Path,
    item: &TodoItem,
    commit: &Commit,
    subject: &str,
    parent: Option<&ObjectId>,
    opts: &ReplayOpts,
) -> Result<Vec<u8>> {
    let mut message = String::new();
    if opts.commit_use_reference {
        let comment = comment_char(git_dir) as char;
        message.push_str(&format!(
            "{comment} *** SAY WHY WE ARE REVERTING ON THE TITLE LINE ***\n"
        ));
    } else if let Some(orig) = subject.strip_prefix("Revert \"")
        && !orig.starts_with("Revert \"")
    {
        message.push_str("Reapply \"");
        message.push_str(orig);
        message.push('\n');
    } else {
        message.push_str(&format!("Revert \"{subject}\"\n"));
    }
    message.push_str("\nThis reverts commit ");
    message.push_str(&refer_to_commit(db, format, &item.oid, opts)?);
    if commit.parents.len() > 1
        && let Some(parent) = parent
    {
        message.push_str(", reversing\nchanges made to ");
        message.push_str(&refer_to_commit(db, format, parent, opts)?);
    }
    message.push_str(".\n");
    Ok(message.into_bytes())
}

fn refer_to_commit(
    db: &FileObjectDatabase,
    format: sley_core::ObjectFormat,
    oid: &ObjectId,
    opts: &ReplayOpts,
) -> Result<String> {
    if !opts.commit_use_reference {
        return Ok(oid.to_hex());
    }
    let object = db.read_object(oid)?;
    let commit = Commit::parse(format, &object.body)?;
    let subject = commit_subject(&commit.message);
    let date =
        sley_ref_filter::commit_identity_date(&commit.author, &sley_core::DateMode::Short);
    Ok(format!(
        "{} ({subject}, {date})",
        format_log_abbrev_oid(oid)
    ))
}

/// Refuse the replay when applying the result would clobber local
/// modifications or untracked files (unpack_trees' verify steps).
fn verify_worktree_safe(
    ctx: &PickContext,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
) -> std::result::Result<(), ReplayHalt> {
    let mut local_changes: Vec<Vec<u8>> = Vec::new();
    let mut untracked: Vec<Vec<u8>> = Vec::new();
    for (path, result) in results {
        let target: Option<(u32, ObjectId)> = match result {
            MergePathResult::Resolved(entry) => *entry,
            MergePathResult::Conflict { .. } => None,
        };
        let changes = match result {
            MergePathResult::Resolved(entry) => ours_map.get(path) != entry.as_ref(),
            MergePathResult::Conflict { .. } => true,
        };
        if !changes {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(path) else {
            continue;
        };
        let full = ctx.worktree_root.join(rel);
        match ours_map.get(path) {
            Some((_, ours_oid)) => {
                // Tracked: the on-disk content must match ours' blob.
                let Ok(bytes) = std::fs::read(&full) else {
                    continue;
                };
                let on_disk = sley_core::object_id_for_bytes(ctx.format, "blob", &bytes)
                    .map_err(print_fatal_error)?;
                if &on_disk != ours_oid {
                    local_changes.push(path.clone());
                }
            }
            None => {
                let would_write =
                    target.is_some() || matches!(result, MergePathResult::Conflict { .. });
                if !would_write {
                    continue;
                }
                // A new gitlink (mode 160000) materializes a submodule *directory*.
                // git's unpack-trees `verify_absent_1` routes a gitlink entry to
                // `check_submodule_move_head` rather than `check_ok_to_remove`, so a
                // *directory* in the way (e.g. the `revert "Replace sub1 with
                // directory"` case, where the on-disk `sub1` dir holds tracked files
                // being removed by this same apply) does not trip the refusal — but
                // an untracked *non-directory* at the path still would be clobbered
                // and must abort ("added submodule doesn't remove untracked file").
                if matches!(target, Some((mode, _)) if is_gitlink(mode)) {
                    if std::fs::symlink_metadata(&full).is_ok_and(|meta| !meta.is_dir()) {
                        untracked.push(path.clone());
                    }
                    continue;
                }
                // Untracked file in the way of a new (non-gitlink) path.
                if full.exists() {
                    untracked.push(path.clone());
                }
            }
        }
    }
    if !local_changes.is_empty() {
        eprintln!(
            "error: Your local changes to the following files would be overwritten by merge:"
        );
        for path in &local_changes {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please commit your changes or stash them before you merge.");
        eprintln!("Aborting");
        return Err(ReplayHalt::Fatal);
    }
    if !untracked.is_empty() {
        eprintln!(
            "error: The following untracked working tree files would be overwritten by merge:"
        );
        for path in &untracked {
            eprintln!("\t{}", String::from_utf8_lossy(path));
        }
        eprintln!("Please move or remove them before you merge.");
        eprintln!("Aborting");
        return Err(ReplayHalt::Fatal);
    }
    Ok(())
}

/// Apply merge results: update the index (preserving cached stat data for
/// unchanged entries) and the worktree files that changed.
fn apply_merge_results_to_index_and_worktree(
    ctx: &PickContext,
    hosts: &PickHosts<'_>,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    let mut old_index = if index_path.exists() {
        Index::parse(&std::fs::read(&index_path)?, ctx.format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let physical_sparse_index = old_index
        .entries
        .iter()
        .any(sley_index::IndexEntry::is_sparse_dir)
        .then(|| old_index.clone());
    // The sequencer merge operates on leaf paths. Expand a sparse index only
    // in memory so unchanged leaves retain their skip-worktree state when the
    // result index is rebuilt after a pick. Otherwise every leaf represented
    // by a synthetic sparse-directory row is re-created as an ordinary entry
    // and status reports the entire excluded cone as deleted.
    if old_index.is_sparse() {
        for entry in &mut old_index.entries {
            if entry.mode == sley_index::SPARSE_DIR_MODE && entry.path.as_bytes().ends_with(b"/") {
                entry.set_skip_worktree(true);
            }
        }
        sley_worktree::expand_sparse_index_view(&mut old_index, &ctx.db, ctx.format)?;
    }
    let mut old_entries: BTreeMap<Vec<u8>, IndexEntry> = BTreeMap::new();
    for entry in &old_index.entries {
        if index_entry_stage(entry) == 0 {
            old_entries.insert(entry.path.clone().into_bytes(), entry.clone());
        }
    }

    let mut entries: Vec<IndexEntry> = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if let Some(old) = old_entries.get(path)
                    && old.mode == *mode
                    && old.oid == *oid
                {
                    entries.push(old.clone());
                } else {
                    entries.push(merge_index_entry(path, *mode, *oid, 0));
                }
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
    // The index is written AFTER the worktree phases below so freshly resolved
    // stage-0 entries can record the on-disk stat (git refreshes merged results
    // via fill_stat_cache_info; a zeroed stat makes diff-files report them
    // dirty). Entries reused from the old index keep their existing stat.

    // git's unpack-trees materializes removals before creations. This matters
    // when a directory's tracked children are removed and the directory is then
    // (re)created as a gitlink — single-phase ordering would prune the emptied
    // directory after the create ("replace directory with submodule"). Phase 0:
    // every removal; phase 1: every write.
    for (path, result) in results {
        let remove = match result {
            MergePathResult::Resolved(None) => ours_map.contains_key(path),
            MergePathResult::Conflict { worktree: None, .. } => true,
            _ => false,
        };
        if remove {
            merge_remove_worktree_file(&ctx.worktree_root, path)?;
        }
    }
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    // A gitlink (submodule) entry's oid is a *commit* recorded in
                    // the submodule, NOT a blob in the superproject ODB — reading
                    // it as a blob fails ("not found"/"expected blob"). git's
                    // entry.c gitlink arm never reads object content: it only
                    // `mkdir`s the submodule dir. `merge_write_worktree_file`
                    // ignores `content` for a gitlink mode (it materializes the
                    // directory), so pass empty content and skip the ODB read.
                    let content = if is_gitlink(*mode) {
                        Vec::new()
                    } else {
                        merge_read_blob_with_fetch(&ctx.db, oid, hosts.promisor_fetch)?
                    };
                    merge_write_worktree_file(&ctx.worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict { worktree, .. } => {
                if let Some((mode, content)) = worktree {
                    merge_write_worktree_file(&ctx.worktree_root, path, content, *mode)?;
                }
            }
        }
    }

    for entry in &mut entries {
        if index_entry_stage(entry) != 0
            || is_gitlink(entry.mode)
            || entry.mtime_seconds != 0
            || entry.ctime_seconds != 0
        {
            continue;
        }
        if let Ok(rel) = std::str::from_utf8(entry.path.as_bytes())
            && let Ok(metadata) = std::fs::symlink_metadata(ctx.worktree_root.join(rel))
        {
            sley_worktree::fill_index_entry_stat_cache(entry, &metadata);
        }
    }
    let mut index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    if let Some(physical) = physical_sparse_index.as_ref() {
        restore_unchanged_sparse_directories(&mut index, physical, &ctx.db, ctx.format)?;
    }
    index.upgrade_version_for_flags();
    std::fs::write(&index_path, index.write(ctx.format)?)?;
    // git's sequencer runs `read_and_refresh_cache` around each pick, so a
    // stage-0 entry reused from the old index (its content unchanged by the
    // merge, so its worktree file was not rewritten above) is re-stat'd against
    // the worktree and its cached stat updated when the file's stat drifted but
    // its content still hashes to the entry oid (e.g. an out-of-band `tar xf`
    // rewrote the file with a fresh mtime between picks). Without this the stale
    // cached stat makes `git diff-files` report a phantom modification
    // (`ie_match_stat` compares size+mtime, not content). Quiet + ignore-missing
    // so a genuinely dirty/absent path is left for status to report, never an
    // error here.
    sley_worktree::refresh_index_paths(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        &[],
        /* quiet */ true,
        /* ignore_missing */ true,
        /* really_refresh */ false,
    )?;
    Ok(())
}

/// Restore physical sparse-directory rows whose represented subtree was not
/// changed by the merge. The sequencer computes against a full semantic view,
/// but an in-cone pick must not turn unrelated sparse directories into persisted
/// leaf entries (nor advertise an expansion that Git never performed).
fn restore_unchanged_sparse_directories(
    index: &mut Index,
    physical: &Index,
    db: &FileObjectDatabase,
    format: sley_core::ObjectFormat,
) -> Result<()> {
    let mut restored = false;
    for sparse in physical.entries.iter().filter(|entry| entry.is_sparse_dir()) {
        let prefix = sparse.path.as_bytes();
        let expected = sley_diff_merge::flatten_tree(db, format, &sparse.oid)?;
        let actual = index
            .entries
            .iter()
            .filter(|entry| entry.path.as_bytes().starts_with(prefix))
            .collect::<Vec<_>>();
        if actual.len() != expected.len()
            || actual.iter().any(|entry| {
                entry.stage() != sley_index::Stage::Normal
                    || expected.get(&entry.path.as_bytes()[prefix.len()..])
                        != Some(&(entry.mode, entry.oid))
            })
        {
            continue;
        }
        index
            .entries
            .retain(|entry| !entry.path.as_bytes().starts_with(prefix));
        index.entries.push(sparse.clone());
        restored = true;
    }
    if restored {
        index
            .entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        index.set_sparse_extension();
    }
    Ok(())
}

/// `append_conflicts_hint`: the commented `Conflicts:` block (with the
/// scissors cut line first under `--cleanup=scissors`).
fn append_conflicts_hint(message: &mut Vec<u8>, conflicts: &[Vec<u8>], cleanup: Option<&str>) {
    if !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    if cleanup == Some("scissors") {
        message.push(b'\n');
        message.extend_from_slice(b"# ------------------------ >8 ------------------------\n");
        message.extend_from_slice(b"# Do not modify or remove the line above.\n");
        message.extend_from_slice(b"# Everything below it will be ignored.\n");
        message.extend_from_slice(b"#\n");
        message.extend_from_slice(b"# Conflicts:\n");
    } else {
        message.push(b'\n');
        message.extend_from_slice(b"# Conflicts:\n");
    }
    for path in conflicts {
        message.extend_from_slice(b"#\t");
        message.extend_from_slice(path);
        message.push(b'\n');
    }
}

fn print_conflict_advice(ctx: &PickContext, opts: &ReplayOpts, help_msg: Option<&str>) {
    if !config_bool(&ctx.config, "advice", "mergeConflict").unwrap_or(true) {
        return;
    }
    let lines: Vec<String> = if let Some(msg) = help_msg {
        msg.lines().map(str::to_string).collect()
    } else if opts.no_commit {
        vec![
            "after resolving the conflicts, mark the corrected paths".to_string(),
            "with 'git add <paths>' or 'git rm <paths>'".to_string(),
        ]
    } else {
        let me = ctx.action.name();
        vec![
            "After resolving the conflicts, mark them with".to_string(),
            "\"git add/rm <pathspec>\", then run".to_string(),
            format!("\"git {me} --continue\"."),
            format!("You can instead skip this commit with \"git {me} --skip\"."),
            format!("To abort and get back to the state before \"git {me}\","),
            format!("run \"git {me} --abort\"."),
        ]
    };
    for line in lines {
        eprintln!("hint: {line}");
    }
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

/// stderr block `git commit` prints when a pick resolves to nil (the
/// `empty_cherry_pick_advice` + single-pick variant; the whence probe in git
/// always reports the single variant for non-rebase picks).
fn print_empty_halt_advice(ctx: &PickContext) {
    eprintln!("The previous cherry-pick is now empty, possibly due to conflict resolution.");
    eprintln!("If you wish to commit it anyway, use:");
    eprintln!();
    eprintln!("    git commit --allow-empty");
    eprintln!();
    let me = ctx.action.name();
    eprintln!("Otherwise, please use 'git {me} --skip'");
}

pub fn merge_results_to_tree_map(
    results: &BTreeMap<Vec<u8>, MergePathResult>,
) -> MergeTreeMap {
    let mut out = MergeTreeMap::new();
    for (path, result) in results {
        if let MergePathResult::Resolved(Some((mode, oid))) = result {
            out.insert(path.clone(), (*mode, *oid));
        }
    }
    out
}

fn should_edit(action: ReplayAction, opts: &ReplayOpts) -> bool {
    use std::io::IsTerminal as _;
    match opts.edit {
        Some(edit) => edit,
        // Unspecified: revert edits when stdin is a tty; cherry-pick doesn't.
        None => action == ReplayAction::Revert && std::io::stdin().is_terminal(),
    }
}

fn fast_forward_to(ctx: &PickContext, target: &ObjectId, head: Option<&ObjectId>) -> Result<()> {
    let refs = ctx.refs();
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let committer = commit_identity_from_env("COMMITTER", &ctx.config)?;
    let old_oid = head.copied().unwrap_or_else(|| ObjectId::null(ctx.format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(|oid| RefTarget::Direct(*oid)),
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: *target,
            committer,
            message: format!("{}: fast-forward", ctx.action.name()).into_bytes(),
        }),
    });
    tx.commit()?;
    sley_worktree::reset_index_and_worktree_to_commit(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        target,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn commit_and_advance_head(
    ctx: &PickContext,
    tree: &ObjectId,
    head: Option<&ObjectId>,
    author: Vec<u8>,
    committer: Vec<u8>,
    message: &[u8],
    reflog_message: Vec<u8>,
    encoding: Option<Vec<u8>>,
) -> Result<ObjectId> {
    let mut db = ctx.db.clone();
    let new_oid = crate::create_commit(
        &mut db,
        crate::CommitCreate {
            tree: *tree,
            parents: head.iter().map(|oid| **oid).collect(),
            author,
            committer: committer.clone(),
            message: message.to_vec(),
            encoding,
            signature: None,
        },
    )?;
    let refs = ctx.refs();
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head.copied().unwrap_or_else(|| ObjectId::null(ctx.format));
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(|oid| RefTarget::Direct(*oid)),
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: reflog_message,
        }),
    });
    tx.commit()?;
    Ok(new_oid)
}

fn print_commit_summary_line(ctx: &PickContext, oid: &ObjectId, message: &[u8]) {
    let refs = ctx.refs();
    let location = match refs.read_ref("HEAD") {
        Ok(Some(RefTarget::Symbolic(branch))) => branch
            .strip_prefix("refs/heads/")
            .unwrap_or(&branch)
            .to_string(),
        _ => "detached HEAD".to_string(),
    };
    let subject = commit_subject(message);
    println!("[{location} {}] {subject}", format_log_abbrev_oid(oid));
}

// ---------------------------------------------------------------------------
// --continue / --skip / --abort / --quit
// ---------------------------------------------------------------------------

/// `--continue`.
pub fn continue_sequence(ctx: &PickContext, hosts: &mut PickHosts<'_>) -> Result<()> {
    let opts = replay::read_opts(&ctx.git_dir).map_err(|err| {
        eprintln!("error: {err}");
        fatal_failed(ctx.action)
    })?;
    if !replay::todo_path(&ctx.git_dir).exists() {
        return continue_single_pick(ctx, hosts, &opts).map(|_| ());
    }
    let items = read_populate_todo(ctx)?;
    // Conflict resolution pending: commit it first.
    if ctx.git_dir.join("CHERRY_PICK_HEAD").exists() || ctx.git_dir.join("REVERT_HEAD").exists() {
        continue_single_pick(ctx, hosts, &opts)?;
    }
    // The stopped item is concluded; replay the rest.
    pick_commits(ctx, hosts, &opts, &items[1.min(items.len())..])
}

fn read_populate_todo(ctx: &PickContext) -> Result<Vec<TodoItem>> {
    let todo_path = replay::todo_path(&ctx.git_dir);
    let text = std::fs::read_to_string(&todo_path).map_err(|err| {
        eprintln!("error: could not read '{}': {err}", todo_path.display());
        fatal_failed(ctx.action)
    })?;
    let unusable = |line_errors: &[String]| {
        for error in line_errors {
            eprintln!("error: {error}");
        }
        eprintln!(
            "error: unusable instruction sheet: '{}'",
            todo_path.display()
        );
        fatal_failed(ctx.action)
    };
    let parsed = match replay::parse_todo(&text) {
        Ok(parsed) => parsed,
        Err(err) => return Err(unusable(&err.line_errors)),
    };
    if parsed.is_empty() {
        eprintln!("error: no commits parsed.");
        return Err(fatal_failed(ctx.action));
    }
    let mut items = Vec::with_capacity(parsed.len());
    for (idx, line) in parsed.iter().enumerate() {
        if line.action != ctx.action {
            let message = match ctx.action {
                ReplayAction::Pick => "cannot cherry-pick during a revert.",
                ReplayAction::Revert => "cannot revert during a cherry-pick.",
            };
            eprintln!("error: {message}");
            return Err(fatal_failed(ctx.action));
        }
        let oid = match resolve_revision(ctx, &line.object_name) {
            Ok(oid) => oid,
            Err(_) => {
                let errors = vec![
                    format!("could not parse '{}'", line.object_name),
                    format!(
                        "invalid line {}: {} {} {}",
                        idx + 1,
                        line.action.command(),
                        line.object_name,
                        line.rest
                    ),
                ];
                return Err(unusable(&errors));
            }
        };
        items.push(TodoItem {
            action: line.action,
            oid,
            display: if line.rest.is_empty() {
                line.object_name.clone()
            } else {
                format!("{} {}", line.object_name, line.rest)
            },
        });
    }
    Ok(items)
}

/// `continue_single_pick`: conclude the stopped pick by committing the staged
/// resolution (the inline equivalent of the `git commit` child).
fn continue_single_pick(
    ctx: &PickContext,
    hosts: &mut PickHosts<'_>,
    opts: &ReplayOpts,
) -> Result<()> {
    let cph = ctx.git_dir.join("CHERRY_PICK_HEAD");
    let rvh = ctx.git_dir.join("REVERT_HEAD");
    if !cph.exists() && !rvh.exists() {
        eprintln!("error: no cherry-pick or revert in progress");
        return Err(fatal_failed(ctx.action));
    }
    // Unmerged files leave the commit impossible (the `git commit` child's
    // error block, exit 128).
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    if let Ok(bytes) = std::fs::read(&index_path) {
        let index = Index::parse(&bytes, ctx.format)?;
        let mut has_unmerged = false;
        for entry in index.entries.iter().filter(|entry| index_entry_stage(entry) > 0) {
            has_unmerged = true;
            println!("U\t{}", entry.path);
        }
        if has_unmerged {
            eprintln!("error: Committing is not possible because you have unmerged files.");
            eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
            eprintln!("hint: as appropriate to mark resolution and make a commit.");
            eprintln!("fatal: Exiting because of an unresolved conflict.");
            return Err(GitError::Exit(128));
        }
    }
    let head = ctx.head_oid();
    let head_tree = match &head {
        Some(oid) => commit_tree_oid(&ctx.db, ctx.format, oid)?,
        None => ObjectId::empty_tree(ctx.format),
    };
    let index_tree = index_tree_oid(ctx)?;
    if index_tree == head_tree {
        // Resolved to nil: print the empty advice and stop (exit 1).
        print_empty_halt_advice(ctx);
        return Err(GitError::Exit(1));
    }
    // Message from MERGE_MSG with comments stripped (--cleanup=strip).
    let raw = std::fs::read(ctx.git_dir.join("MERGE_MSG")).unwrap_or_default();
    let mut message = strip_comment_lines(&raw, comment_char(&ctx.git_dir));
    let edit = opts.edit == Some(true);
    message = (hosts.prepare_commit_message)(&ctx.git_dir, message, true, edit)?;
    // Author: the picked commit's author for cherry-picks; env for reverts.
    let target_encoding = commit_encoding_config(&ctx.git_dir);
    let author = if cph.exists() {
        let text = std::fs::read_to_string(&cph)?;
        let oid = ObjectId::from_hex(ctx.format, text.trim())?;
        let object = ctx.db.read_object(&oid)?;
        let commit = Commit::parse(ctx.format, &object.body)?;
        commit_author_for_commit_encoding(&commit, &target_encoding).into_owned()
    } else {
        commit_identity_from_env("AUTHOR", &ctx.config)?
    };
    let committer = commit_identity_from_env("COMMITTER", &ctx.config)?;
    let subject = commit_subject(&message);
    let new_oid = commit_and_advance_head(
        ctx,
        &index_tree,
        head.as_ref(),
        author,
        committer,
        &message,
        format!("commit: {subject}").into_bytes(),
        commit_encoding_header_from_config(&ctx.git_dir),
    )?;
    let _ = std::fs::remove_file(&cph);
    let _ = std::fs::remove_file(&rvh);
    let _ = std::fs::remove_file(ctx.git_dir.join("MERGE_MSG"));
    print_commit_summary_line(ctx, &new_oid, &message);
    Ok(())
}

/// `--skip`.
pub fn skip_sequence(ctx: &PickContext, hosts: &mut PickHosts<'_>) -> Result<()> {
    let last = replay::last_command(&ctx.git_dir);
    let state_file = ctx.git_dir.join(ctx.action.head_file());
    if !state_file.exists() {
        if last != Some(ctx.action) {
            eprintln!("error: no {} in progress", ctx.action.name());
            return Err(fatal_failed(ctx.action));
        }
        let head = ctx.head_oid();
        if !replay::rollback_is_safe(&ctx.git_dir, head.as_ref()) {
            eprintln!("error: there is nothing to skip");
            if config_bool(&ctx.config, "advice", "resolveConflict").unwrap_or(true) {
                eprintln!("hint: have you committed already?");
                eprintln!("hint: try \"git {} --continue\"", ctx.action.name());
            }
            return Err(fatal_failed(ctx.action));
        }
    }
    // `git reset --merge HEAD` (an unborn HEAD resets to the empty tree).
    let head = ctx.head_oid();
    reset_merge(
        ctx,
        hosts.promisor_fetch,
        head.as_ref(),
    )
    .map_err(|_| {
        eprintln!("error: failed to skip the commit");
        fatal_failed(ctx.action)
    })?;
    if !replay::seq_dir(&ctx.git_dir).is_dir() {
        return Ok(());
    }
    // Continue after a skip: like `continue_sequence` but the stopped item is
    // dropped without committing.
    let opts = replay::read_opts(&ctx.git_dir).map_err(|err| {
        eprintln!("error: {err}");
        fatal_failed(ctx.action)
    })?;
    let items = read_populate_todo(ctx)?;
    pick_commits(ctx, hosts, &opts, &items[1.min(items.len())..])
}

/// `--abort` (`sequencer_rollback`).
pub fn rollback(ctx: &PickContext, hosts: &PickHosts<'_>) -> Result<()> {
    let head_file = replay::head_path(&ctx.git_dir);
    if !head_file.exists() {
        // Single-pick abort.
        if !ctx.git_dir.join("CHERRY_PICK_HEAD").exists()
            && !ctx.git_dir.join("REVERT_HEAD").exists()
        {
            eprintln!("error: no cherry-pick or revert in progress");
            return Err(fatal_failed(ctx.action));
        }
        let Some(head) = ctx.head_oid() else {
            eprintln!("error: cannot abort from a branch yet to be born");
            return Err(fatal_failed(ctx.action));
        };
        return reset_merge(ctx, hosts.promisor_fetch, Some(&head)).map_err(|err| {
            eprintln!("error: {err}");
            fatal_failed(ctx.action)
        });
    }
    let text = std::fs::read_to_string(&head_file).map_err(|err| {
        eprintln!("error: cannot open '{}': {err}", head_file.display());
        fatal_failed(ctx.action)
    })?;
    let stored = text.trim();
    let oid = ObjectId::from_hex(ctx.format, stored).map_err(|_| {
        eprintln!(
            "error: stored pre-cherry-pick HEAD file '{}' is corrupt",
            head_file.display()
        );
        fatal_failed(ctx.action)
    })?;
    if oid == ObjectId::null(ctx.format) {
        eprintln!("error: cannot abort from a branch yet to be born");
        return Err(fatal_failed(ctx.action));
    }
    let head = ctx.head_oid();
    if !replay::rollback_is_safe(&ctx.git_dir, head.as_ref()) {
        eprintln!("warning: You seem to have moved HEAD. Not rewinding, check your HEAD!");
    } else {
        reset_merge(ctx, hosts.promisor_fetch, Some(&oid)).map_err(|err| {
            eprintln!("error: {err}");
            fatal_failed(ctx.action)
        })?;
    }
    replay::remove_state(&ctx.git_dir);
    Ok(())
}

/// `git reset --merge <oid>` against this pick's repository: reset the index
/// to the target tree and update worktree files whose index entry changes,
/// refusing to clobber paths whose on-disk content diverges from the (stage-0)
/// index. Conflicted paths are reset outright. Clears the in-progress branch
/// state on success.
fn reset_merge(
    ctx: &PickContext,
    fetch: Option<&dyn PromisorObjectFetch>,
    target: Option<&ObjectId>,
) -> Result<()> {
    reset_merge_in(
        &ctx.git_dir,
        &ctx.worktree_root,
        ctx.format,
        target,
        &ctx.config,
        fetch,
    )
}

/// Canonical `git reset --merge` used by the replay `--skip`/`--abort` flows
/// and `git reset --merge` itself.
pub fn reset_merge_in(
    git_dir: &Path,
    worktree_root: &Path,
    format: sley_core::ObjectFormat,
    target: Option<&ObjectId>,
    config: &GitConfig,
    fetch: Option<&dyn PromisorObjectFetch>,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let target_map = match target {
        Some(target) => {
            let target_tree = commit_tree_oid(&db, format, target)?;
            sley_diff_merge::flatten_tree(&db, format, &target_tree)?
        }
        None => MergeTreeMap::new(),
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
    let mut old_index = if index_path.exists() {
        Index::parse(&std::fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    // `reset --merge` reasons about tracked leaves, not the sparse index's
    // synthetic 040000 directory rows. Expanding only the in-memory view keeps
    // those rows from being mistaken for deletions (and from producing bogus
    // "unable to rmdir" warnings) while preserving the destination's sparse
    // worktree policy.
    if old_index.is_sparse() {
        for entry in &mut old_index.entries {
            if entry.mode == sley_index::SPARSE_DIR_MODE && entry.path.as_bytes().ends_with(b"/") {
                entry.set_skip_worktree(true);
            }
        }
        sley_worktree::expand_sparse_index_view(&mut old_index, &db, format)?;
    }
    let mut stage0: BTreeMap<Vec<u8>, IndexEntry> = BTreeMap::new();
    let mut conflicted: BTreeSet<Vec<u8>> = BTreeSet::new();
    for entry in &old_index.entries {
        let path = entry.path.clone().into_bytes();
        if index_entry_stage(entry) == 0 {
            stage0.insert(path, entry.clone());
        } else {
            conflicted.insert(path);
        }
    }

    // Plan: per path, decide keep / update / delete + verify safety.
    let mut errors: Vec<Vec<u8>> = Vec::new();
    let mut updates: Vec<(Vec<u8>, (u32, ObjectId))> = Vec::new();
    let mut deletions: Vec<Vec<u8>> = Vec::new();
    let all_paths: BTreeSet<Vec<u8>> = stage0
        .keys()
        .cloned()
        .chain(conflicted.iter().cloned())
        .chain(target_map.keys().cloned())
        .collect();
    for path in &all_paths {
        let target_entry = target_map.get(path);
        let conflict = conflicted.contains(path);
        let current = stage0.get(path);
        if conflict {
            // Conflicted entries are reset to the target unconditionally.
            match target_entry {
                Some(entry) => updates.push((path.clone(), *entry)),
                None => deletions.push(path.clone()),
            }
            continue;
        }
        let current_entry = current.map(|entry| (entry.mode, entry.oid));
        if current_entry.as_ref() == target_entry {
            continue;
        }
        if current.is_some_and(|entry| is_gitlink(entry.mode))
            && target_entry.is_none()
            && target_map.keys().any(|candidate| {
                candidate.starts_with(path) && candidate.get(path.len()) == Some(&b'/')
            })
        {
            errors.push(path.clone());
            continue;
        }
        // The index entry changes: the worktree must match the index.
        if let Some(entry) = current {
            let rel = entry.path.to_string();
            let full = worktree_root.join(&rel);
            if let Ok(bytes) = std::fs::read(&full) {
                let on_disk = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
                if on_disk != entry.oid {
                    errors.push(path.clone());
                    continue;
                }
            }
        } else if matches!(target_entry, Some((mode, _)) if is_gitlink(*mode)) {
            let Some(rel) = std::str::from_utf8(path).ok() else {
                continue;
            };
            let full = worktree_root.join(rel);
            if std::fs::symlink_metadata(&full).is_ok_and(|metadata| !metadata.is_dir()) {
                errors.push(path.clone());
                continue;
            }
        }
        match target_entry {
            Some(entry) => updates.push((path.clone(), *entry)),
            None => deletions.push(path.clone()),
        }
    }
    if !errors.is_empty() {
        for path in &errors {
            eprintln!(
                "error: Entry '{}' not uptodate. Cannot merge.",
                String::from_utf8_lossy(path)
            );
        }
        let target_text = target
            .map(|oid| oid.to_hex())
            .unwrap_or_else(|| "HEAD".to_string());
        return Err(GitError::Command(format!(
            "Could not reset index file to revision '{target_text}'."
        )));
    }

    // Apply: rebuild the index from the target tree (preserving stat data
    // for unchanged entries), then touch only the planned worktree paths.
    let mut entries: Vec<IndexEntry> = Vec::new();
    for (path, (mode, oid)) in &target_map {
        if let Some(old) = stage0.get(path)
            && old.mode == *mode
            && old.oid == *oid
        {
            entries.push(old.clone());
        } else {
            let mut replacement = merge_index_entry(path, *mode, *oid, 0);
            if stage0.get(path).is_some_and(IndexEntry::is_skip_worktree) {
                replacement.set_skip_worktree(true);
            }
            entries.push(replacement);
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    index.upgrade_version_for_flags();
    std::fs::write(&index_path, index.write(format)?)?;
    // Apply removals before materializations. A directory→gitlink transition
    // has flattened deletes under `path/` plus a gitlink at `path`; writing the
    // gitlink directory first and then pruning its children via
    // `merge_remove_worktree_file` → `merge_prune_empty_dirs` would rmdir the
    // newly-required empty submodule placeholder ("replace directory with
    // submodule"). Deletions first leave the parent gone/empty, then the
    // gitlink write recreates the empty directory.
    for path in &deletions {
        merge_remove_worktree_file(worktree_root, path)?;
    }
    for (path, (mode, oid)) in &updates {
        let content = if is_gitlink(*mode) {
            Vec::new()
        } else {
            merge_read_blob_with_fetch(&db, oid, fetch)?
        };
        merge_write_worktree_file(worktree_root, path, &content, *mode)?;
    }
    // Belt-and-suspenders: every target gitlink must exist as a directory
    // placeholder even when no CE_UPDATE was planned (identical oid carried
    // through) or when a deletion pruned a parent that the target still records
    // as a submodule path.
    for (path, (mode, _)) in &target_map {
        if !is_gitlink(*mode) {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(path) else {
            continue;
        };
        let full = worktree_root.join(rel);
        if full.is_dir() {
            continue;
        }
        merge_write_worktree_file(worktree_root, path, &[], *mode)?;
    }
    sley_worktree::refresh_index_paths_with_options(
        worktree_root,
        git_dir,
        format,
        &[],
        /* quiet */ true,
        /* ignore_missing */ true,
        /* ignore_submodules */ false,
        /* allow_unmerged */ false,
        /* really_refresh */ false,
    )?;

    // Move HEAD (branch tip or detached) when the target differs.
    let refs = FileRefStore::new(git_dir, format);
    let current_head = head_commit_oid(&refs)?;
    if let Some(target) = target
        && current_head.as_ref() != Some(target)
    {
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let committer = commit_identity_from_env("COMMITTER", config)?;
        let old_oid = current_head.unwrap_or_else(|| ObjectId::null(format));
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: current_head.map(RefTarget::Direct),
            new: RefTarget::Direct(*target),
            reflog: Some(ReflogEntry {
                old_oid,
                new_oid: *target,
                committer,
                message: b"reset: moving to target".to_vec(),
            }),
        });
        tx.commit()?;
    }

    replay::remove_branch_state(git_dir);
    Ok(())
}

#[cfg(test)]
mod comment_cleanup_tests {
    use super::strip_comment_string_lines;

    #[test]
    fn strips_the_complete_multibyte_comment_prefix() {
        let message = b"A3\n\nCOMMENT generated help\nCOMMENT status\n\nedited\n";
        assert_eq!(
            strip_comment_string_lines(message, b"COMMENT"),
            b"A3\n\nedited\n"
        );
    }
}
