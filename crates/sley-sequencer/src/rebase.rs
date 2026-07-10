//! The interactive-rebase todo-list state machine (`.git/rebase-merge/`).
//!
//! Owns the on-disk contract of git's `sequencer.c` for the rebase half: the
//! todo instruction sheet (parse + serialize, including the editor help
//! block), the `rebase-merge` state files (`done`, `msgnum`, `end`,
//! `head-name`, `onto`, `orig-head`, `amend`, `stopped-sha`, `autostash`,
//! `author-script`, fixup/squash message scratch files), and the
//! `author-script` quoting rules. The drive loop (merging trees, committing,
//! editors) lives with the CLI porcelain.

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_odb::{FileObjectDatabase, ObjectPrefixResolution};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// User-requested history-editing action after CLI option parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEditAction {
    Start,
    Continue,
    Abort,
    Skip,
    Quit,
    EditTodo,
    ShowCurrentPatch,
}

/// On-disk backend selected for a resumed history edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEditBackend {
    Apply,
    Merge,
}

/// Inputs for choosing the backend that owns a history-editing invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryEditPlanOptions {
    pub action: HistoryEditAction,
    pub apply_in_progress: bool,
    pub merge_in_progress: bool,
}

/// Backend-selection outcome before porcelain execution begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEditPlan {
    Start,
    Resume {
        backend: HistoryEditBackend,
        action: HistoryEditAction,
    },
    MissingState,
}

/// Select the history-editing backend from explicit action and on-disk state.
#[must_use]
pub fn plan_history_edit(options: HistoryEditPlanOptions) -> HistoryEditPlan {
    let backend = if options.apply_in_progress {
        Some(HistoryEditBackend::Apply)
    } else if options.merge_in_progress {
        Some(HistoryEditBackend::Merge)
    } else {
        None
    };
    match backend {
        Some(backend) => HistoryEditPlan::Resume {
            backend,
            action: options.action,
        },
        None if options.action == HistoryEditAction::Start => HistoryEditPlan::Start,
        None => HistoryEditPlan::MissingState,
    }
}

/// `todo_command_info` order matters: parsing tries commands in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoCommand {
    Pick,
    Revert,
    Edit,
    Reword,
    Fixup,
    Squash,
    Exec,
    Break,
    Label,
    Reset,
    Merge,
    UpdateRef,
    Noop,
    Drop,
    Comment,
}

/// `TODO_EDIT_MERGE_MSG`
pub const FLAG_EDIT_MERGE_MSG: u8 = 1 << 0;
/// `TODO_REPLACE_FIXUP_MSG` (`fixup -C`)
pub const FLAG_REPLACE_FIXUP_MSG: u8 = 1 << 1;
/// `TODO_EDIT_FIXUP_MSG` (`fixup -c`)
pub const FLAG_EDIT_FIXUP_MSG: u8 = 1 << 2;

impl TodoCommand {
    const ORDER: [TodoCommand; 14] = [
        TodoCommand::Pick,
        TodoCommand::Revert,
        TodoCommand::Edit,
        TodoCommand::Reword,
        TodoCommand::Fixup,
        TodoCommand::Squash,
        TodoCommand::Exec,
        TodoCommand::Break,
        TodoCommand::Label,
        TodoCommand::Reset,
        TodoCommand::Merge,
        TodoCommand::UpdateRef,
        TodoCommand::Noop,
        TodoCommand::Drop,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TodoCommand::Pick => "pick",
            TodoCommand::Revert => "revert",
            TodoCommand::Edit => "edit",
            TodoCommand::Reword => "reword",
            TodoCommand::Fixup => "fixup",
            TodoCommand::Squash => "squash",
            TodoCommand::Exec => "exec",
            TodoCommand::Break => "break",
            TodoCommand::Label => "label",
            TodoCommand::Reset => "reset",
            TodoCommand::Merge => "merge",
            TodoCommand::UpdateRef => "update-ref",
            TodoCommand::Noop => "noop",
            TodoCommand::Drop => "drop",
            TodoCommand::Comment => "comment",
        }
    }

    pub fn nick(self) -> Option<char> {
        match self {
            TodoCommand::Pick => Some('p'),
            TodoCommand::Edit => Some('e'),
            TodoCommand::Reword => Some('r'),
            TodoCommand::Fixup => Some('f'),
            TodoCommand::Squash => Some('s'),
            TodoCommand::Exec => Some('x'),
            TodoCommand::Break => Some('b'),
            TodoCommand::Label => Some('l'),
            TodoCommand::Reset => Some('t'),
            TodoCommand::Merge => Some('m'),
            TodoCommand::UpdateRef => Some('u'),
            TodoCommand::Drop => Some('d'),
            TodoCommand::Revert | TodoCommand::Noop | TodoCommand::Comment => None,
        }
    }

    /// `is_noop`: commands at or after `TODO_NOOP` in the upstream enum.
    pub fn is_noop(self) -> bool {
        matches!(
            self,
            TodoCommand::Noop | TodoCommand::Drop | TodoCommand::Comment
        )
    }

    pub fn is_fixup(self) -> bool {
        matches!(self, TodoCommand::Fixup | TodoCommand::Squash)
    }

    /// Creates a (non-merge) commit.
    pub fn is_pick_or_similar(self) -> bool {
        matches!(
            self,
            TodoCommand::Pick
                | TodoCommand::Revert
                | TodoCommand::Edit
                | TodoCommand::Reword
                | TodoCommand::Fixup
                | TodoCommand::Squash
        )
    }
}

/// One parsed instruction-sheet entry. `raw` preserves the exact line bytes
/// (without the newline) so `save_todo` / `done` writes stay byte-faithful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseTodoItem {
    pub command: TodoCommand,
    pub flags: u8,
    /// Resolved commit for commands that name one.
    pub oid: Option<ObjectId>,
    /// The argument text after the object name (or the full argument for
    /// exec/label/reset/merge-without-commit/update-ref; the line text for
    /// comments).
    pub arg: String,
    pub raw: String,
}

impl RebaseTodoItem {
    pub fn comment(line: &str) -> Self {
        RebaseTodoItem {
            command: TodoCommand::Comment,
            flags: 0,
            oid: None,
            arg: line.to_string(),
            raw: line.to_string(),
        }
    }
}

/// Outcome of resolving an object name on a todo line.
pub enum TodoOidLookup {
    /// Resolved; `parents` is the commit's parent count (merge detection).
    Commit { oid: ObjectId, parents: usize },
    /// Name did not resolve to a commit.
    Missing,
}

/// A formatted message produced while parsing (printed verbatim by the
/// porcelain, already carrying the `error: ` / `hint: ` prefix).
pub type TodoParseMessages = Vec<String>;

/// `is_command`: full command word or one-char nick followed by
/// space/tab/EOL; returns the remainder.
fn strip_todo_command(bol: &str, command: TodoCommand) -> Option<&str> {
    let word = command.as_str();
    let separator_ok = |rest: &str| rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r']);
    if let Some(rest) = bol.strip_prefix(word)
        && separator_ok(rest)
    {
        return Some(rest);
    }
    if let Some(nick) = command.nick() {
        let mut chars = bol.chars();
        if chars.next() == Some(nick) {
            let rest = chars.as_str();
            if separator_ok(rest) {
                return Some(rest);
            }
        }
    }
    None
}

/// Parse the instruction sheet (`todo_list_parse_insn_buffer` +
/// `parse_insn_line`). `resolve` maps an object-name token to a commit.
/// Returns the items plus the ordered error/hint lines; an empty error list
/// means the sheet parsed cleanly. Items that failed to parse are recorded as
/// comments so totals/offsets still line up with upstream.
pub fn parse_todo_buffer(
    text: &str,
    done_exists: bool,
    comment_char: char,
    resolve: &mut dyn FnMut(&str) -> TodoOidLookup,
) -> (Vec<RebaseTodoItem>, TodoParseMessages) {
    let mut items = Vec::new();
    let mut messages = Vec::new();
    let mut fixup_okay = done_exists;
    let mut line_number = 0usize;
    for raw_line in text.split('\n') {
        line_number += 1;
        // `split` yields a final empty piece after a trailing newline; skip
        // it (upstream iterates `*p` and stops at NUL).
        if raw_line.is_empty() && text.split('\n').count() == line_number {
            break;
        }
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        match parse_todo_line(line, comment_char, resolve, &mut messages) {
            Ok(item) => {
                if !fixup_okay && item.command.is_fixup() {
                    messages.push(format!(
                        "error: cannot '{}' without a previous commit",
                        item.command.as_str()
                    ));
                } else if !item.command.is_noop() {
                    fixup_okay = true;
                }
                items.push(item);
            }
            Err(()) => {
                messages.push(format!("error: invalid line {line_number}: {line}"));
                items.push(RebaseTodoItem::comment(line));
            }
        }
    }
    (items, messages)
}

fn parse_todo_line(
    line: &str,
    comment_char: char,
    resolve: &mut dyn FnMut(&str) -> TodoOidLookup,
    messages: &mut TodoParseMessages,
) -> std::result::Result<RebaseTodoItem, ()> {
    let bol = line.trim_start_matches([' ', '\t']);
    if bol.is_empty() || bol.starts_with(comment_char) {
        return Ok(RebaseTodoItem::comment(line));
    }
    let mut matched = None;
    for command in TodoCommand::ORDER {
        if let Some(rest) = strip_todo_command(bol, command) {
            matched = Some((command, rest));
            break;
        }
    }
    let Some((command, rest)) = matched else {
        let token: String = bol
            .chars()
            .take_while(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
            .collect();
        messages.push(format!("error: invalid command '{token}'"));
        return Err(());
    };

    let padding = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let mut bol = rest.trim_start_matches([' ', '\t']);

    if matches!(command, TodoCommand::Noop | TodoCommand::Break) {
        if !bol.is_empty() {
            messages.push(format!(
                "error: {} does not accept arguments: '{bol}'",
                command.as_str()
            ));
            return Err(());
        }
        return Ok(RebaseTodoItem {
            command,
            flags: 0,
            oid: None,
            arg: String::new(),
            raw: line.to_string(),
        });
    }

    if padding == 0 {
        messages.push(format!("error: missing arguments for {}", command.as_str()));
        return Err(());
    }

    if command == TodoCommand::Label {
        if !valid_label(bol) {
            messages.push(format!("error: '{}' is not a valid label", bol));
            return Err(());
        }
        return Ok(RebaseTodoItem {
            command,
            flags: 0,
            oid: None,
            arg: bol.to_string(),
            raw: line.to_string(),
        });
    }

    if command == TodoCommand::UpdateRef {
        if !bol.starts_with("refs/") {
            if !valid_refname(bol, true) {
                messages.push(format!("error: '{}' is not a valid refname", bol));
            } else {
                messages.push(
                    "error: update-ref requires a fully qualified refname e.g. refs/heads/topic"
                        .to_string(),
                );
            }
            return Err(());
        }
        if !valid_refname(bol, false) {
            messages.push(format!("error: '{}' is not a valid refname", bol));
            return Err(());
        }
        return Ok(RebaseTodoItem {
            command,
            flags: 0,
            oid: None,
            arg: bol.to_string(),
            raw: line.to_string(),
        });
    }

    if matches!(command, TodoCommand::Exec | TodoCommand::Reset) {
        return Ok(RebaseTodoItem {
            command,
            flags: 0,
            oid: None,
            arg: bol.to_string(),
            raw: line.to_string(),
        });
    }

    let mut flags = 0u8;
    if command == TodoCommand::Fixup {
        if let Some(rest) = bol.strip_prefix("-C") {
            bol = rest.trim_start_matches([' ', '\t']);
            flags |= FLAG_REPLACE_FIXUP_MSG;
        } else if let Some(rest) = bol.strip_prefix("-c") {
            bol = rest.trim_start_matches([' ', '\t']);
            flags |= FLAG_EDIT_FIXUP_MSG;
        }
    }
    if command == TodoCommand::Merge {
        if let Some(rest) = bol.strip_prefix("-C") {
            bol = rest.trim_start_matches([' ', '\t']);
        } else if let Some(rest) = bol.strip_prefix("-c") {
            bol = rest.trim_start_matches([' ', '\t']);
            flags |= FLAG_EDIT_MERGE_MSG;
        } else {
            return Ok(RebaseTodoItem {
                command,
                flags: FLAG_EDIT_MERGE_MSG,
                oid: None,
                arg: bol.to_string(),
                raw: line.to_string(),
            });
        }
    }

    let end = bol.find([' ', '\t', '\n']).unwrap_or(bol.len());
    let (object_name, tail) = bol.split_at(end);
    let arg = tail.trim_start_matches([' ', '\t']).to_string();
    match resolve(object_name) {
        TodoOidLookup::Commit { oid, parents } => {
            if parents > 1 && !matches!(command, TodoCommand::Merge | TodoCommand::Drop) {
                push_merge_commit_messages(command, messages);
                return Err(());
            }
            Ok(RebaseTodoItem {
                command,
                flags,
                oid: Some(oid),
                arg,
                raw: line.to_string(),
            })
        }
        TodoOidLookup::Missing => {
            messages.push(format!("error: could not parse '{object_name}'"));
            Err(())
        }
    }
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label != "#"
        && !label.starts_with(':')
        && !label.contains('/')
        && !label.contains("..")
        && !label.contains("@{")
        && !label.ends_with('.')
        && !label.ends_with(".lock")
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_refname(refname: &str, allow_onelevel: bool) -> bool {
    if refname.is_empty()
        || refname.starts_with('/')
        || refname.ends_with('/')
        || refname.contains("..")
        || refname.contains("@{")
        || refname.ends_with('.')
        || refname.ends_with(".lock")
    {
        return false;
    }
    let mut components = 0usize;
    for component in refname.split('/') {
        components += 1;
        if component.is_empty()
            || component.starts_with('.')
            || component.ends_with(".lock")
            || component.bytes().any(|b| {
                b < 0x20
                    || b == 0x7f
                    || matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
            })
        {
            return false;
        }
    }
    allow_onelevel || components >= 2
}

/// `check_merge_commit_insn`: the error + advice when a pick-like command
/// names a merge commit.
fn push_merge_commit_messages(command: TodoCommand, messages: &mut TodoParseMessages) {
    match command {
        TodoCommand::Pick => {
            messages.push("error: 'pick' does not accept merge commits".to_string());
            for line in [
                "'pick' does not take a merge commit. If you wanted to",
                "replay the merge, use 'merge -C' on the commit.",
            ] {
                messages.push(format!("hint: {line}"));
            }
            push_todo_error_disable_hint(messages);
        }
        TodoCommand::Reword => {
            messages.push("error: 'reword' does not accept merge commits".to_string());
            for line in [
                "'reword' does not take a merge commit. If you wanted to",
                "replay the merge and reword the commit message, use",
                "'merge -c' on the commit",
            ] {
                messages.push(format!("hint: {line}"));
            }
            push_todo_error_disable_hint(messages);
        }
        TodoCommand::Edit => {
            messages.push("error: 'edit' does not accept merge commits".to_string());
            for line in [
                "'edit' does not take a merge commit. If you wanted to",
                "replay the merge, use 'merge -C' on the commit, and then",
                "'break' to give the control back to you so that you can",
                "do 'git commit --amend && git rebase --continue'.",
            ] {
                messages.push(format!("hint: {line}"));
            }
            push_todo_error_disable_hint(messages);
        }
        TodoCommand::Fixup | TodoCommand::Squash => {
            messages.push("error: cannot squash merge commit into another commit".to_string());
        }
        _ => {}
    }
}

fn push_todo_error_disable_hint(messages: &mut TodoParseMessages) {
    messages.push(
        "hint: Disable this message with \"git config set advice.rebaseTodoError false\""
            .to_string(),
    );
}

/// Serialize one todo item (`todo_list_to_strbuf` per-item logic).
/// `oid_text` renders the commit id (short or full).
pub fn todo_item_to_string(item: &RebaseTodoItem, oid_text: Option<&str>) -> String {
    if item.command == TodoCommand::Comment {
        return item.arg.clone();
    }
    let mut out = String::from(item.command.as_str());
    if let Some(oid) = oid_text {
        if item.command == TodoCommand::Fixup {
            if item.flags & FLAG_EDIT_FIXUP_MSG != 0 {
                out.push_str(" -c");
            } else if item.flags & FLAG_REPLACE_FIXUP_MSG != 0 {
                out.push_str(" -C");
            }
        }
        if item.command == TodoCommand::Merge {
            if item.flags & FLAG_EDIT_MERGE_MSG != 0 {
                out.push_str(" -c");
            } else {
                out.push_str(" -C");
            }
        }
        out.push(' ');
        out.push_str(oid);
    }
    if !item.arg.is_empty() {
        out.push(' ');
        out.push_str(&item.arg);
    }
    out
}

/// The command legend appended below the todo list (`append_todo_help`).
const TODO_HELP_COMMANDS: &str = "\
\nCommands:
p, pick <commit> = use commit
r, reword <commit> = use commit, but edit the commit message
e, edit <commit> = use commit, but stop for amending
s, squash <commit> = use commit, but meld into previous commit
f, fixup [-C | -c] <commit> = like \"squash\" but keep only the previous
                   commit's log message, unless -C is used, in which case
                   keep only this commit's message; -c is same as -C but
                   opens the editor
x, exec <command> = run command (the rest of the line) using shell
b, break = stop here (continue rebase later with 'git rebase --continue')
d, drop <commit> = remove commit
l, label <label> = label current HEAD with a name
t, reset <label> = reset HEAD to a label
m, merge [-C <commit> | -c <commit>] <label> [# <oneline>]
        create a merge commit using the original merge commit's
        message (or the oneline, if no original merge commit was
        specified); use -c <commit> to reword the commit message
u, update-ref <ref> = track a placeholder for the <ref> to be updated
                      to this position in the new commits. The <ref> is
                      updated at the end of the rebase

These lines can be re-ordered; they are executed from top to bottom.
";

fn add_commented_lines(buf: &mut String, text: &str, comment: char) {
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n');
        let content = body.unwrap_or(line);
        if content.is_empty() {
            buf.push(comment);
        } else {
            buf.push(comment);
            buf.push(' ');
            buf.push_str(content);
        }
        buf.push('\n');
    }
}

/// `append_todo_help`. `shortrevisions`/`shortonto` present means the
/// initial-edit variant (with the `Rebase a..b onto c` headline);
/// absent means the `--edit-todo` variant. `check_level_error` selects the
/// "Do not remove any line" warning text.
pub fn append_todo_help(
    buf: &mut String,
    command_count: usize,
    shortrevisions: Option<&str>,
    shortonto: Option<&str>,
    comment: char,
    check_level_error: bool,
) {
    let edit_todo = !(shortrevisions.is_some() && shortonto.is_some());
    if !edit_todo {
        buf.push('\n');
        let plural = if command_count == 1 {
            "command"
        } else {
            "commands"
        };
        buf.push(comment);
        buf.push(' ');
        buf.push_str(&format!(
            "Rebase {} onto {} ({command_count} {plural})\n",
            shortrevisions.unwrap_or_default(),
            shortonto.unwrap_or_default()
        ));
    }
    add_commented_lines(buf, TODO_HELP_COMMANDS, comment);
    let msg = if check_level_error {
        "\nDo not remove any line. Use 'drop' explicitly to remove a commit.\n"
    } else {
        "\nIf you remove a line here THAT COMMIT WILL BE LOST.\n"
    };
    add_commented_lines(buf, msg, comment);
    let msg = if edit_todo {
        "\nYou are editing the todo file of an ongoing interactive rebase.\nTo continue rebase after editing, run:\n    git rebase --continue\n\n"
    } else {
        "\nHowever, if you remove everything, the rebase will be aborted.\n\n"
    };
    add_commented_lines(buf, msg, comment);
}

// ---------------------------------------------------------------------------
// State directory
// ---------------------------------------------------------------------------

pub fn merge_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("rebase-merge")
}

pub fn state_path(git_dir: &Path, name: &str) -> PathBuf {
    merge_dir(git_dir).join(name)
}

pub fn in_progress(git_dir: &Path) -> bool {
    merge_dir(git_dir).is_dir()
}

/// Read a single-line state file, trimming the trailing newline.
pub fn read_state_line(git_dir: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(state_path(git_dir, name)).ok()?;
    Some(text.trim_end_matches('\n').to_string())
}

pub fn write_state_file(git_dir: &Path, name: &str, contents: &str) -> std::io::Result<()> {
    fs::write(state_path(git_dir, name), contents)
}

/// Append one completed instruction to `rebase-merge/done`.
///
/// The sequencer advances this file once per todo command.  Opening it in
/// append mode avoids reading and rewriting the complete history for every
/// step while preserving the exact line-oriented on-disk contract.
pub fn append_done_line(git_dir: &Path, line: &[u8]) -> std::io::Result<()> {
    let mut done = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_path(git_dir, "done"))?;
    done.write_all(line)?;
    done.write_all(b"\n")
}

pub fn remove_merge_state(git_dir: &Path) {
    let _ = fs::remove_dir_all(merge_dir(git_dir));
}

/// Repository-level state for an interactive rebase merge backend.
///
/// This is the typed counterpart of Git's line- and marker-oriented
/// `rebase-merge` directory.  Porcelain decides which options to select; the
/// sequencer owns their durable representation so embedders can start and
/// resume the same operation without reproducing CLI-specific file wiring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseState {
    pub quiet: bool,
    pub verbose: bool,
    pub signoff: bool,
    pub allow_ff: bool,
    pub drop_redundant_commits: bool,
    pub keep_redundant_commits: bool,
    pub reschedule_failed_exec: bool,
    pub committer_date_is_author_date: bool,
    pub ignore_date: bool,
    pub gpg_sign: Option<String>,
    pub no_gpg_sign: bool,
    pub strategy: Option<String>,
    pub strategy_opts: Vec<String>,
    pub rerere_autoupdate: Option<bool>,
    pub head_name: Option<String>,
    pub onto: ObjectId,
    pub orig_head: ObjectId,
    pub squash_onto: Option<ObjectId>,
}

/// Persist a rebase merge-backend state using Git's on-disk contract.
pub fn write_rebase_state(git_dir: &Path, state: &RebaseState) -> Result<()> {
    let dir = merge_dir(git_dir);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("interactive"), b"")?;
    let head_name = state.head_name.as_deref().unwrap_or("detached HEAD");
    fs::write(dir.join("head-name"), format!("{head_name}\n"))?;
    fs::write(dir.join("onto"), format!("{}\n", state.onto))?;
    fs::write(dir.join("orig-head"), format!("{}\n", state.orig_head))?;
    if let Some(squash_onto) = state.squash_onto {
        fs::write(dir.join("squash-onto"), format!("{squash_onto}\n"))?;
    }
    if state.quiet {
        fs::write(dir.join("quiet"), b"")?;
    }
    if state.verbose {
        fs::write(dir.join("verbose"), b"")?;
    }
    if state.signoff {
        fs::write(dir.join("signoff"), b"--signoff\n")?;
    }
    if state.drop_redundant_commits {
        fs::write(dir.join("drop_redundant_commits"), b"")?;
    }
    if state.keep_redundant_commits {
        fs::write(dir.join("keep_redundant_commits"), b"")?;
    }
    fs::write(
        dir.join(if state.reschedule_failed_exec {
            "reschedule-failed-exec"
        } else {
            "no-reschedule-failed-exec"
        }),
        b"",
    )?;
    if state.committer_date_is_author_date {
        fs::write(dir.join("cdate_is_adate"), b"")?;
    }
    if state.ignore_date {
        fs::write(dir.join("ignore_date"), b"")?;
    }
    if let Some(key) = &state.gpg_sign {
        let value = if key.is_empty() {
            "-S".to_string()
        } else {
            format!("-S{key}")
        };
        fs::write(dir.join("gpg_sign_opt"), format!("{value}\n"))?;
    }
    if state.no_gpg_sign {
        fs::write(dir.join("no_gpg_sign"), b"")?;
    }
    if let Some(strategy) = &state.strategy {
        fs::write(dir.join("strategy"), format!("{strategy}\n"))?;
    }
    if !state.strategy_opts.is_empty() {
        fs::write(
            dir.join("strategy_opts"),
            format!("{}\n", sq_quote_argv(&state.strategy_opts)),
        )?;
    }
    match state.rerere_autoupdate {
        Some(true) => fs::write(
            dir.join("allow_rerere_autoupdate"),
            b"--rerere-autoupdate\n",
        )?,
        Some(false) => fs::write(
            dir.join("allow_rerere_autoupdate"),
            b"--no-rerere-autoupdate\n",
        )?,
        None => {}
    }
    Ok(())
}

/// Load a rebase merge-backend state written by Sley or Git.
pub fn read_rebase_state(git_dir: &Path, format: ObjectFormat) -> Result<RebaseState> {
    let head_name = read_state_line(git_dir, "head-name")
        .ok_or_else(|| GitError::not_found("rebase-merge/head-name"))?;
    let onto_raw =
        read_state_line(git_dir, "onto").ok_or_else(|| GitError::not_found("rebase-merge/onto"))?;
    let onto = ObjectId::from_hex(format, onto_raw.trim())
        .map_err(|_| GitError::InvalidObject("invalid onto value during rebase".into()))?;
    let orig_raw = read_state_line(git_dir, "orig-head")
        .ok_or_else(|| GitError::not_found("rebase-merge/orig-head"))?;
    let orig_head = ObjectId::from_hex(format, orig_raw.trim())
        .map_err(|_| GitError::InvalidObject("invalid orig-head value during rebase".into()))?;
    let squash_onto = match read_state_line(git_dir, "squash-onto") {
        Some(raw) => Some(ObjectId::from_hex(format, raw.trim()).map_err(|_| {
            GitError::InvalidObject("invalid squash-onto value during rebase".into())
        })?),
        None => None,
    };
    let exists = |name: &str| state_path(git_dir, name).exists();
    let signoff = exists("signoff");
    let gpg_sign = read_state_line(git_dir, "gpg_sign_opt")
        .and_then(|value| value.strip_prefix("-S").map(str::to_string));
    let strategy = read_state_line(git_dir, "strategy");
    let strategy_opts = fs::read_to_string(state_path(git_dir, "strategy_opts"))
        .map(|text| parse_strategy_opts(&text))
        .unwrap_or_default();
    let rerere_autoupdate = match read_state_line(git_dir, "allow_rerere_autoupdate") {
        Some(line) if line.contains("--no-rerere-autoupdate") => Some(false),
        Some(line) if line.contains("--rerere-autoupdate") => Some(true),
        _ => None,
    };
    Ok(RebaseState {
        quiet: exists("quiet"),
        verbose: exists("verbose"),
        signoff,
        allow_ff: !signoff,
        drop_redundant_commits: exists("drop_redundant_commits"),
        keep_redundant_commits: exists("keep_redundant_commits"),
        reschedule_failed_exec: exists("reschedule-failed-exec"),
        committer_date_is_author_date: exists("cdate_is_adate"),
        ignore_date: exists("ignore_date"),
        gpg_sign,
        no_gpg_sign: exists("no_gpg_sign"),
        strategy,
        strategy_opts,
        rerere_autoupdate,
        head_name: head_name.starts_with("refs/").then_some(head_name),
        onto,
        orig_head,
        squash_onto,
    })
}

fn sq_quote_argv(args: &[String]) -> String {
    let mut out = Vec::new();
    for arg in args {
        out.push(b' ');
        out.extend_from_slice(&sq_quote_bytes(arg.as_bytes()));
    }
    String::from_utf8(out).expect("quoting UTF-8 arguments preserves UTF-8")
}

fn parse_strategy_opts(text: &str) -> Vec<String> {
    if !text.trim_start().starts_with('\'') {
        return text.lines().map(str::to_string).collect();
    }
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        if bytes[index] != b'\'' {
            let start = index;
            while bytes
                .get(index)
                .is_some_and(|byte| !byte.is_ascii_whitespace())
            {
                index += 1;
            }
            tokens.push(String::from_utf8_lossy(&bytes[start..index]).into_owned());
            continue;
        }

        let mut token = Vec::new();
        let mut source = index;
        loop {
            source += 1;
            let Some(&byte) = bytes.get(source) else {
                break;
            };
            if byte != b'\'' {
                token.push(byte);
                continue;
            }
            source += 1;
            if bytes.get(source) == Some(&b'\\')
                && bytes.get(source + 2) == Some(&b'\'')
                && matches!(bytes.get(source + 1), Some(b'\'' | b'!'))
            {
                token.push(bytes[source + 1]);
                source += 2;
                continue;
            }
            break;
        }
        index = source;
        tokens.push(String::from_utf8_lossy(&token).into_owned());
    }
    tokens
}

/// Object-id and command abbreviation policy for rendering a todo list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TodoRenderOptions {
    /// Render full object names when absent, or unique names no shorter than
    /// this width when present.
    pub minimum_abbrev: Option<usize>,
    /// Use one-letter command nicks where Git permits them.
    pub abbreviate_commands: bool,
}

/// Count executable instructions, excluding comments and blank lines.
pub fn count_commands(items: &[RebaseTodoItem]) -> usize {
    items
        .iter()
        .filter(|item| item.command != TodoCommand::Comment)
        .count()
}

/// Find the shortest unambiguous object name at least `minimum` characters
/// wide using the repository's object database.
pub fn unique_abbrev(db: &FileObjectDatabase, oid: &ObjectId, minimum: usize) -> String {
    let hex = oid.to_hex();
    let mut width = minimum.min(hex.len());
    while width < hex.len() {
        match db.resolve_prefix(&hex[..width]) {
            Ok(ObjectPrefixResolution::Ambiguous(_)) => width += 1,
            _ => break,
        }
    }
    hex[..width].to_string()
}

/// Render one rebase instruction according to repository abbreviation policy.
pub fn render_todo_item(
    db: &FileObjectDatabase,
    item: &RebaseTodoItem,
    options: TodoRenderOptions,
) -> String {
    let oid_text = item.oid.as_ref().map(|oid| match options.minimum_abbrev {
        Some(width) => unique_abbrev(db, oid, width),
        None => oid.to_hex(),
    });
    let mut text = todo_item_to_string(item, oid_text.as_deref());
    if options.abbreviate_commands
        && item.command != TodoCommand::Comment
        && let Some(nick) = item.command.nick()
        && let Some(rest) = text.strip_prefix(item.command.as_str())
    {
        text = format!("{nick}{rest}");
    }
    text
}

/// Render a complete line-oriented rebase instruction sheet.
pub fn render_todo_list(
    db: &FileObjectDatabase,
    items: &[RebaseTodoItem],
    options: TodoRenderOptions,
) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(&render_todo_item(db, item, options));
        out.push('\n');
    }
    out
}

/// Durable progress counters and instructions for a running rebase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseTodoList {
    pub items: Vec<RebaseTodoItem>,
    /// Index of the instruction currently being executed.
    pub current: usize,
    /// Number of executable instructions already recorded in `done`.
    pub done_nr: usize,
    /// `done_nr` plus the remaining executable instructions.
    pub total_nr: usize,
}

/// Result of loading an instruction sheet. Invalid user-edited todo files are
/// data, not I/O failures, so porcelain can render Git-compatible advice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadTodoListOutcome {
    Ready(RebaseTodoList),
    Invalid { messages: TodoParseMessages },
}

/// Load the active todo list, account for completed commands, and update the
/// persisted `end` counter.
pub fn load_rebase_todo_list(
    git_dir: &Path,
    comment_char: char,
    resolve: &mut dyn FnMut(&str) -> TodoOidLookup,
) -> Result<LoadTodoListOutcome> {
    let text = fs::read_to_string(state_path(git_dir, "git-rebase-todo"))?;
    let done_exists = state_path(git_dir, "done").exists();
    let (items, messages) = parse_todo_buffer(&text, done_exists, comment_char, resolve);
    if !messages.is_empty() {
        return Ok(LoadTodoListOutcome::Invalid { messages });
    }
    let done_nr = fs::read_to_string(state_path(git_dir, "done"))
        .map(|text| {
            let (done_items, _) = parse_todo_buffer(&text, true, comment_char, resolve);
            count_commands(&done_items)
        })
        .unwrap_or(0);
    let total_nr = done_nr + count_commands(&items);
    fs::write(state_path(git_dir, "end"), format!("{total_nr}\n"))?;
    Ok(LoadTodoListOutcome::Ready(RebaseTodoList {
        items,
        current: 0,
        done_nr,
        total_nr,
    }))
}

/// Advance (or reschedule) the active instruction and persist both the todo
/// tail and completed command using full object names.
pub fn save_rebase_todo_list(
    git_dir: &Path,
    db: &FileObjectDatabase,
    todo: &RebaseTodoList,
    reschedule: bool,
) -> Result<()> {
    let next = if reschedule {
        todo.current
    } else {
        todo.current + 1
    };
    let tail = if next <= todo.items.len() {
        &todo.items[next..]
    } else {
        &[]
    };
    let options = TodoRenderOptions {
        minimum_abbrev: None,
        abbreviate_commands: false,
    };
    fs::write(
        state_path(git_dir, "git-rebase-todo"),
        render_todo_list(db, tail, options),
    )?;
    if !reschedule && next > 0 {
        let line = render_todo_item(db, &todo.items[next - 1], options);
        append_done_line(git_dir, line.as_bytes())?;
    }
    Ok(())
}

/// `sq_quote_buf`: wrap in single quotes, escaping embedded quotes and bangs
/// (`'` becomes `'\''`).
fn sq_quote_bytes(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'\'');
    for byte in value {
        if *byte == b'\'' || *byte == b'!' {
            out.push(b'\'');
            out.push(b'\\');
            out.push(*byte);
            out.push(b'\'');
        } else {
            out.push(*byte);
        }
    }
    out.push(b'\'');
    out
}

/// `write_author_script`: persist the stopped commit's author identity.
/// `author` is the raw `Name <email> ts tz` identity line.
pub fn format_author_script(author: &[u8]) -> Option<Vec<u8>> {
    let fields = sley_core::split_ident_line(author)?;
    let date = fields.date?;
    let tz = fields.tz?;
    let mut out = b"GIT_AUTHOR_NAME=".to_vec();
    out.extend_from_slice(&sq_quote_bytes(fields.name));
    out.extend_from_slice(b"\nGIT_AUTHOR_EMAIL=");
    out.extend_from_slice(&sq_quote_bytes(fields.email));
    out.extend_from_slice(b"\nGIT_AUTHOR_DATE=");
    let mut raw_date = b"@".to_vec();
    raw_date.extend_from_slice(date);
    raw_date.push(b' ');
    raw_date.extend_from_slice(tz);
    out.extend_from_slice(&sq_quote_bytes(&raw_date));
    out.push(b'\n');
    Some(out)
}

/// Parse `author-script` back into the raw identity pieces
/// (name, email, `@ts tz` date).
pub fn parse_author_script_bytes(text: &[u8]) -> Option<(Vec<u8>, Vec<u8>, String)> {
    let mut name = None;
    let mut email = None;
    let mut date = None;
    for line in text
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let equals = line.iter().position(|byte| *byte == b'=')?;
        let (key, value) = line.split_at(equals);
        let value = sq_dequote_bytes(&value[1..])?;
        match key {
            b"GIT_AUTHOR_NAME" => name = Some(value),
            b"GIT_AUTHOR_EMAIL" => email = Some(value),
            b"GIT_AUTHOR_DATE" => date = Some(String::from_utf8(value).ok()?),
            _ => return None,
        }
    }
    Some((name?, email?, date?))
}

pub fn parse_author_script(text: &str) -> Option<(String, String, String)> {
    let (name, email, date) = parse_author_script_bytes(text.as_bytes())?;
    Some((
        String::from_utf8(name).ok()?,
        String::from_utf8(email).ok()?,
        date,
    ))
}

fn sq_dequote_bytes(value: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut index = 0usize;
    if value.get(index) != Some(&b'\'') {
        return None;
    }
    index += 1;
    loop {
        let byte = *value.get(index)?;
        index += 1;
        if byte == b'\'' {
            match value.get(index) {
                None => return Some(out),
                Some(b'\\') => {
                    index += 1;
                    let escaped = *value.get(index)?;
                    index += 1;
                    out.push(escaped);
                    if value.get(index) != Some(&b'\'') {
                        return None;
                    }
                    index += 1;
                }
                Some(_) => return None,
            }
        } else {
            out.push(byte);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, hex).expect("test operation should succeed")
    }

    fn resolver(token: &str) -> TodoOidLookup {
        if token.len() >= 7 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
            TodoOidLookup::Commit {
                oid: oid("21b83cd2e8f4d6d8d9615779ebaa801ba891eb04"),
                parents: 1,
            }
        } else {
            TodoOidLookup::Missing
        }
    }

    #[test]
    fn parses_commands_and_nicks() {
        let text = "pick 21b83cd # one\nr 21b83cd # two\nbreak\nexec make test\n# comment\n\ndrop 21b83cd # three\n";
        let (items, messages) = parse_todo_buffer(text, false, '#', &mut resolver);
        assert!(messages.is_empty(), "{messages:?}");
        let commands: Vec<TodoCommand> = items.iter().map(|item| item.command).collect();
        assert_eq!(
            commands,
            vec![
                TodoCommand::Pick,
                TodoCommand::Reword,
                TodoCommand::Break,
                TodoCommand::Exec,
                TodoCommand::Comment,
                TodoCommand::Comment,
                TodoCommand::Drop,
            ]
        );
        assert_eq!(items[0].arg, "# one");
        assert_eq!(items[3].arg, "make test");
        assert_eq!(items[0].raw, "pick 21b83cd # one");
    }

    #[test]
    fn flags_bad_lines_in_order() {
        let (_, messages) = parse_todo_buffer("pickled 21b83cd # x\n", false, '#', &mut resolver);
        assert_eq!(
            messages,
            vec![
                "error: invalid command 'pickled'".to_string(),
                "error: invalid line 1: pickled 21b83cd # x".to_string(),
            ]
        );
        let (_, messages) = parse_todo_buffer("pick nope # x\n", false, '#', &mut resolver);
        assert_eq!(
            messages,
            vec![
                "error: could not parse 'nope'".to_string(),
                "error: invalid line 1: pick nope # x".to_string(),
            ]
        );
        let (_, messages) = parse_todo_buffer("fixup 21b83cd # x\n", false, '#', &mut resolver);
        assert_eq!(
            messages,
            vec!["error: cannot 'fixup' without a previous commit".to_string()]
        );
    }

    #[test]
    fn fixup_flags_parse() {
        let (items, messages) = parse_todo_buffer(
            "pick 21b83cd # a\nfixup -C 21b83cd # b\nfixup -c 21b83cd # c\n",
            false,
            '#',
            &mut resolver,
        );
        assert!(messages.is_empty());
        assert_eq!(items[1].flags, FLAG_REPLACE_FIXUP_MSG);
        assert_eq!(items[2].flags, FLAG_EDIT_FIXUP_MSG);
        assert_eq!(
            todo_item_to_string(&items[1], Some("21b83cd")),
            "fixup -C 21b83cd # b"
        );
    }

    #[test]
    fn validates_labels_and_update_refs() {
        let (_, messages) = parse_todo_buffer(
            "label #\nlabel :invalid\nupdate-ref :bad\nupdate-ref topic\nupdate-ref refs/heads/topic\n",
            false,
            '#',
            &mut resolver,
        );
        assert_eq!(
            messages,
            vec![
                "error: '#' is not a valid label".to_string(),
                "error: invalid line 1: label #".to_string(),
                "error: ':invalid' is not a valid label".to_string(),
                "error: invalid line 2: label :invalid".to_string(),
                "error: ':bad' is not a valid refname".to_string(),
                "error: invalid line 3: update-ref :bad".to_string(),
                "error: update-ref requires a fully qualified refname e.g. refs/heads/topic"
                    .to_string(),
                "error: invalid line 4: update-ref topic".to_string(),
            ]
        );
    }

    #[test]
    fn todo_help_initial_variant() {
        let mut buf = String::new();
        append_todo_help(&mut buf, 2, Some("123..456"), Some("123"), '#', false);
        assert!(buf.starts_with("\n# Rebase 123..456 onto 123 (2 commands)\n"));
        assert!(buf.contains("# p, pick <commit> = use commit\n"));
        assert!(buf.contains("# However, if you remove everything, the rebase will be aborted.\n"));
        assert!(buf.ends_with("#\n"));
    }

    #[test]
    fn author_script_round_trips() {
        let script = format_author_script(b"A U Thor <a@example.com> 1234567890 +0100")
            .expect("test operation should succeed");
        assert_eq!(
            script,
            b"GIT_AUTHOR_NAME='A U Thor'\nGIT_AUTHOR_EMAIL='a@example.com'\nGIT_AUTHOR_DATE='@1234567890 +0100'\n"
        );
        let (name, email, date) =
            parse_author_script_bytes(&script).expect("test operation should succeed");
        assert_eq!(name, b"A U Thor");
        assert_eq!(email, b"a@example.com");
        assert_eq!(date, "@1234567890 +0100");
    }

    #[test]
    fn author_script_preserves_non_utf8_name_bytes() {
        let script = format_author_script(b"\xC1\xE9\xED \xF3\xFA <a@example.com> 1 +0000")
            .expect("format non-utf8 author script");
        let (name, email, date) =
            parse_author_script_bytes(&script).expect("parse non-utf8 author script");
        assert_eq!(name, b"\xC1\xE9\xED \xF3\xFA");
        assert_eq!(email, b"a@example.com");
        assert_eq!(date, "@1 +0000");
    }

    #[test]
    fn done_lines_append_without_rewriting_prior_commands() {
        let root = std::env::temp_dir().join(format!("sley-sequencer-done-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(merge_dir(&root)).expect("create rebase state");
        append_done_line(&root, b"pick 111 first").expect("append first done line");
        append_done_line(&root, b"exec make test").expect("append second done line");
        assert_eq!(
            fs::read(state_path(&root, "done")).expect("read done state"),
            b"pick 111 first\nexec make test\n"
        );
        fs::remove_dir_all(root).expect("remove rebase state");
    }

    #[test]
    fn rebase_state_round_trips_git_files_and_quoted_strategy_options() {
        let root = tempfile::tempdir().expect("create temporary repository");
        let onto = oid("1111111111111111111111111111111111111111");
        let orig_head = oid("2222222222222222222222222222222222222222");
        let squash_onto = oid("3333333333333333333333333333333333333333");
        let state = RebaseState {
            quiet: true,
            verbose: true,
            signoff: true,
            allow_ff: false,
            drop_redundant_commits: true,
            keep_redundant_commits: true,
            reschedule_failed_exec: true,
            committer_date_is_author_date: true,
            ignore_date: true,
            gpg_sign: Some(String::new()),
            no_gpg_sign: true,
            strategy: Some("ort".to_string()),
            strategy_opts: vec![
                "ours".to_string(),
                "has space".to_string(),
                "it's!".to_string(),
                String::new(),
            ],
            rerere_autoupdate: Some(false),
            head_name: Some("refs/heads/topic".to_string()),
            onto,
            orig_head,
            squash_onto: Some(squash_onto),
        };

        write_rebase_state(root.path(), &state).expect("write rebase state");
        assert_eq!(
            fs::read_to_string(state_path(root.path(), "strategy_opts"))
                .expect("read strategy options"),
            " 'ours' 'has space' 'it'\\''s'\\!'\u{27} ''\n"
        );
        assert_eq!(
            fs::read_to_string(state_path(root.path(), "head-name")).expect("read head-name"),
            "refs/heads/topic\n"
        );
        assert_eq!(
            read_rebase_state(root.path(), ObjectFormat::Sha1).expect("read rebase state"),
            state
        );
    }

    #[test]
    fn rebase_state_reads_legacy_line_oriented_strategy_options() {
        let root = tempfile::tempdir().expect("create temporary repository");
        fs::create_dir_all(merge_dir(root.path())).expect("create rebase state");
        fs::write(state_path(root.path(), "head-name"), "detached HEAD\n").expect("write head");
        fs::write(
            state_path(root.path(), "onto"),
            "1111111111111111111111111111111111111111\n",
        )
        .expect("write onto");
        fs::write(
            state_path(root.path(), "orig-head"),
            "2222222222222222222222222222222222222222\n",
        )
        .expect("write orig-head");
        fs::write(state_path(root.path(), "strategy_opts"), "ours\npatience\n")
            .expect("write legacy strategy options");

        let state =
            read_rebase_state(root.path(), ObjectFormat::Sha1).expect("read legacy rebase state");
        assert_eq!(state.head_name, None);
        assert_eq!(state.strategy_opts, ["ours", "patience"]);
        assert!(state.allow_ff);
    }

    #[test]
    fn todo_render_policy_is_owned_by_the_sequencer() {
        let root = tempfile::tempdir().expect("create temporary repository");
        fs::create_dir_all(root.path().join("objects")).expect("create object store");
        let db = FileObjectDatabase::from_git_dir(root.path(), ObjectFormat::Sha1);
        let item = RebaseTodoItem {
            command: TodoCommand::Pick,
            flags: 0,
            oid: Some(oid("21b83cd2e8f4d6d8d9615779ebaa801ba891eb04")),
            arg: "subject".to_string(),
            raw: String::new(),
        };
        assert_eq!(
            render_todo_list(
                &db,
                &[item],
                TodoRenderOptions {
                    minimum_abbrev: Some(7),
                    abbreviate_commands: true,
                },
            ),
            "p 21b83cd subject\n"
        );
    }

    #[test]
    fn todo_list_load_and_advance_own_progress_files() {
        let root = tempfile::tempdir().expect("create temporary repository");
        fs::create_dir_all(merge_dir(root.path())).expect("create rebase state");
        fs::create_dir_all(root.path().join("objects")).expect("create object store");
        fs::write(
            state_path(root.path(), "git-rebase-todo"),
            "pick 21b83cd subject\nexec make test\n",
        )
        .expect("write todo");
        fs::write(state_path(root.path(), "done"), "pick 21b83cd prior\n").expect("write done");

        let mut resolve = resolver;
        let LoadTodoListOutcome::Ready(todo) =
            load_rebase_todo_list(root.path(), '#', &mut resolve).expect("load todo")
        else {
            panic!("expected valid todo list");
        };
        assert_eq!(todo.done_nr, 1);
        assert_eq!(todo.total_nr, 3);
        assert_eq!(
            fs::read_to_string(state_path(root.path(), "end")).expect("read end"),
            "3\n"
        );

        let db = FileObjectDatabase::from_git_dir(root.path(), ObjectFormat::Sha1);
        save_rebase_todo_list(root.path(), &db, &todo, false).expect("advance todo");
        assert_eq!(
            fs::read_to_string(state_path(root.path(), "git-rebase-todo"))
                .expect("read advanced todo"),
            "exec make test\n"
        );
        assert_eq!(
            fs::read_to_string(state_path(root.path(), "done")).expect("read advanced done"),
            "pick 21b83cd prior\npick 21b83cd2e8f4d6d8d9615779ebaa801ba891eb04 subject\n"
        );
    }

    #[test]
    fn invalid_todo_is_a_typed_outcome_and_does_not_advance_state() {
        let root = tempfile::tempdir().expect("create temporary repository");
        fs::create_dir_all(merge_dir(root.path())).expect("create rebase state");
        fs::write(
            state_path(root.path(), "git-rebase-todo"),
            "pick not-an-object subject\n",
        )
        .expect("write todo");
        let mut resolve = resolver;
        let outcome = load_rebase_todo_list(root.path(), '#', &mut resolve).expect("load todo");
        let LoadTodoListOutcome::Invalid { messages } = outcome else {
            panic!("expected invalid todo list");
        };
        assert_eq!(
            messages,
            [
                "error: could not parse 'not-an-object'",
                "error: invalid line 1: pick not-an-object subject",
            ]
        );
        assert!(!state_path(root.path(), "end").exists());
    }

    #[test]
    fn history_edit_plan_prefers_apply_state_and_reports_missing_state() {
        assert_eq!(
            plan_history_edit(HistoryEditPlanOptions {
                action: HistoryEditAction::Continue,
                apply_in_progress: true,
                merge_in_progress: true,
            }),
            HistoryEditPlan::Resume {
                backend: HistoryEditBackend::Apply,
                action: HistoryEditAction::Continue,
            }
        );
        assert_eq!(
            plan_history_edit(HistoryEditPlanOptions {
                action: HistoryEditAction::Abort,
                apply_in_progress: false,
                merge_in_progress: false,
            }),
            HistoryEditPlan::MissingState
        );
        assert_eq!(
            plan_history_edit(HistoryEditPlanOptions {
                action: HistoryEditAction::Start,
                apply_in_progress: true,
                merge_in_progress: false,
            }),
            HistoryEditPlan::Resume {
                backend: HistoryEditBackend::Apply,
                action: HistoryEditAction::Start,
            }
        );
    }
}
