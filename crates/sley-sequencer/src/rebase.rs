//! The interactive-rebase todo-list state machine (`.git/rebase-merge/`).
//!
//! Owns the on-disk contract of git's `sequencer.c` for the rebase half: the
//! todo instruction sheet (parse + serialize, including the editor help
//! block), the `rebase-merge` state files (`done`, `msgnum`, `end`,
//! `head-name`, `onto`, `orig-head`, `amend`, `stopped-sha`, `autostash`,
//! `author-script`, fixup/squash message scratch files), and the
//! `author-script` quoting rules. The drive loop (merging trees, committing,
//! editors) lives with the CLI porcelain.

use sley_core::ObjectId;
use std::fs;
use std::path::{Path, PathBuf};

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

    fn nick(self) -> Option<char> {
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
#[derive(Debug, Clone)]
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
    let separator_ok =
        |rest: &str| rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r']);
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
        messages.push(format!(
            "error: missing arguments for {}",
            command.as_str()
        ));
        return Err(());
    }

    if matches!(
        command,
        TodoCommand::Exec | TodoCommand::Label | TodoCommand::Reset | TodoCommand::UpdateRef
    ) {
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
            if parents > 1 {
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
            messages
                .push("error: cannot squash merge commit into another commit".to_string());
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

pub fn remove_merge_state(git_dir: &Path) {
    let _ = fs::remove_dir_all(merge_dir(git_dir));
}

/// `sq_quote_buf`: wrap in single quotes, escaping embedded quotes
/// (`'` becomes `'\''`).
fn sq_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' || c == '!' {
            out.push('\'');
            out.push('\\');
            out.push(c);
            out.push('\'');
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// `write_author_script`: persist the stopped commit's author identity.
/// `author` is the raw `Name <email> ts tz` identity line.
pub fn format_author_script(author: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(author);
    let open = text.find('<')?;
    let close = text[open..].find('>')? + open;
    let name = text[..open].trim_end();
    let email = &text[open + 1..close];
    let date = text[close + 1..].trim();
    Some(format!(
        "GIT_AUTHOR_NAME={}\nGIT_AUTHOR_EMAIL={}\nGIT_AUTHOR_DATE={}\n",
        sq_quote(name),
        sq_quote(email),
        sq_quote(&format!("@{date}"))
    ))
}

/// Parse `author-script` back into the raw identity pieces
/// (name, email, `@ts tz` date).
pub fn parse_author_script(text: &str) -> Option<(String, String, String)> {
    let mut name = None;
    let mut email = None;
    let mut date = None;
    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        let value = sq_dequote(value)?;
        match key {
            "GIT_AUTHOR_NAME" => name = Some(value),
            "GIT_AUTHOR_EMAIL" => email = Some(value),
            "GIT_AUTHOR_DATE" => date = Some(value),
            _ => return None,
        }
    }
    Some((name?, email?, date?))
}

fn sq_dequote(value: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    if chars.next()? != '\'' {
        return None;
    }
    loop {
        let c = chars.next()?;
        if c == '\'' {
            match chars.peek() {
                None => return Some(out),
                Some('\\') => {
                    chars.next();
                    let escaped = chars.next()?;
                    out.push(escaped);
                    if chars.next()? != '\'' {
                        return None;
                    }
                }
                Some(_) => return None,
            }
        } else {
            out.push(c);
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
        let (items, messages) =
            parse_todo_buffer("pick 21b83cd # a\nfixup -C 21b83cd # b\nfixup -c 21b83cd # c\n", false, '#', &mut resolver);
        assert!(messages.is_empty());
        assert_eq!(items[1].flags, FLAG_REPLACE_FIXUP_MSG);
        assert_eq!(items[2].flags, FLAG_EDIT_FIXUP_MSG);
        assert_eq!(
            todo_item_to_string(&items[1], Some("21b83cd")),
            "fixup -C 21b83cd # b"
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
            "GIT_AUTHOR_NAME='A U Thor'\nGIT_AUTHOR_EMAIL='a@example.com'\nGIT_AUTHOR_DATE='@1234567890 +0100'\n"
        );
        let (name, email, date) =
            parse_author_script(&script).expect("test operation should succeed");
        assert_eq!(name, "A U Thor");
        assert_eq!(email, "a@example.com");
        assert_eq!(date, "@1234567890 +0100");
    }
}
