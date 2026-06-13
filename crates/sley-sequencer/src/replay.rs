//! The cherry-pick / revert sequencer state machine.
//!
//! Owns the on-disk contract of git's `sequencer.c` for the non-rebase replay
//! actions: the `.git/sequencer/` directory (`todo`, `opts`, `head`,
//! `abort-safety`), the single-commit `CHERRY_PICK_HEAD` / `REVERT_HEAD`
//! state files, and the parse/serialize rules for each. The porcelain drive
//! loop (merging trees, committing results) lives with the CLI; everything
//! that reads or writes sequencer state goes through here.

use sley_core::{GitError, ObjectId, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Which replay action a sequencer run performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayAction {
    Pick,
    Revert,
}

impl ReplayAction {
    /// The porcelain name (`cherry-pick` / `revert`) used in messages.
    pub fn name(self) -> &'static str {
        match self {
            ReplayAction::Pick => "cherry-pick",
            ReplayAction::Revert => "revert",
        }
    }

    /// The todo-sheet command word (`pick` / `revert`).
    pub fn command(self) -> &'static str {
        match self {
            ReplayAction::Pick => "pick",
            ReplayAction::Revert => "revert",
        }
    }

    /// The single-commit state file (`CHERRY_PICK_HEAD` / `REVERT_HEAD`).
    pub fn head_file(self) -> &'static str {
        match self {
            ReplayAction::Pick => "CHERRY_PICK_HEAD",
            ReplayAction::Revert => "REVERT_HEAD",
        }
    }
}

/// Replay options, mirroring git's `struct replay_opts` for the subset the
/// cherry-pick / revert porcelains persist in `.git/sequencer/opts`.
#[derive(Debug, Clone, Default)]
pub struct ReplayOpts {
    pub no_commit: bool,
    /// Tri-state `--edit`: `None` = unspecified (git's `edit < 0`).
    pub edit: Option<bool>,
    pub allow_empty: bool,
    pub allow_empty_message: bool,
    pub drop_redundant_commits: bool,
    pub keep_redundant_commits: bool,
    pub signoff: bool,
    /// `-x`: append `(cherry picked from commit ...)`.
    pub record_origin: bool,
    pub allow_ff: bool,
    /// `-m <parent-number>`; 0 = unset.
    pub mainline: u32,
    pub strategy: Option<String>,
    pub gpg_sign: Option<String>,
    /// `-X` strategy options.
    pub strategy_options: Vec<String>,
    /// `--rerere-autoupdate` / `--no-rerere-autoupdate`.
    pub allow_rerere_auto: Option<bool>,
    /// `--cleanup=<mode>` (persisted as `options.default-msg-cleanup`).
    pub default_msg_cleanup: Option<String>,
    /// `--reference` (revert only; not persisted by git).
    pub commit_use_reference: bool,
}

/// One instruction-sheet entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub action: ReplayAction,
    pub oid: ObjectId,
    /// Short display oid + optional subject as written on the sheet.
    pub display: String,
}

pub fn seq_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("sequencer")
}

pub fn todo_path(git_dir: &Path) -> PathBuf {
    seq_dir(git_dir).join("todo")
}

pub fn opts_path(git_dir: &Path) -> PathBuf {
    seq_dir(git_dir).join("opts")
}

pub fn head_path(git_dir: &Path) -> PathBuf {
    seq_dir(git_dir).join("head")
}

pub fn abort_safety_path(git_dir: &Path) -> PathBuf {
    seq_dir(git_dir).join("abort-safety")
}

/// Probe the first instruction of an existing todo sheet, mirroring
/// `sequencer_get_last_command`: `Some(action)` when the sheet's first
/// non-blank line starts with `pick ` / `revert ` (or their abbreviations).
pub fn last_command(git_dir: &Path) -> Option<ReplayAction> {
    let buf = fs::read(todo_path(git_dir)).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let trimmed = text.trim_start_matches([' ', '\t', '\r', '\n']);
    for (action, nick) in [(ReplayAction::Pick, "p"), (ReplayAction::Revert, "")] {
        let full = action.command();
        if let Some(rest) = trimmed.strip_prefix(full)
            && rest.starts_with([' ', '\t'])
        {
            return Some(action);
        }
        if !nick.is_empty()
            && let Some(rest) = trimmed.strip_prefix(nick)
            && rest.starts_with([' ', '\t'])
        {
            return Some(action);
        }
    }
    None
}

/// Create `.git/sequencer/`, failing when a sequence is already in progress
/// (mirrors `create_seq_dir`). On the in-progress case the error/hint pair is
/// printed by the caller; this returns the message text so the porcelain owns
/// stderr ordering.
pub struct InProgress {
    pub error: String,
    pub hint: String,
}

/// Check for an in-progress sequence; `advise_skip` selects the hint variant
/// that offers `--skip`.
pub fn in_progress_error(git_dir: &Path, advise_skip: bool) -> Option<InProgress> {
    let action = last_command(git_dir)?;
    let skip = if advise_skip { "--skip | " } else { "" };
    Some(InProgress {
        error: format!("{} is already in progress", action.name()),
        hint: format!(
            "try \"git {} (--continue | {}--abort | --quit)\"",
            action.name(),
            skip
        ),
    })
}

/// Create the sequencer directory (caller has already ruled out in-progress).
pub fn create_seq_dir(git_dir: &Path) -> Result<()> {
    fs::create_dir(seq_dir(git_dir)).map_err(|err| {
        GitError::Command(format!(
            "could not create sequencer directory '{}': {err}",
            seq_dir(git_dir).display()
        ))
    })
}

/// Persist the pre-sequence HEAD (`.git/sequencer/head`).
pub fn save_head(git_dir: &Path, head: &str) -> Result<()> {
    fs::write(head_path(git_dir), format!("{head}\n"))?;
    Ok(())
}

/// Read the pre-sequence HEAD back; `None` when no sequence recorded one.
pub fn read_head(git_dir: &Path) -> Option<String> {
    let buf = fs::read_to_string(head_path(git_dir)).ok()?;
    Some(buf.lines().next().unwrap_or("").to_string())
}

/// Record the current HEAD in `abort-safety` (mirrors
/// `update_abort_safety_file`; a no-op without a sequencer dir). `head` is
/// `None` when HEAD is unresolvable, recorded as an empty file.
pub fn update_abort_safety(git_dir: &Path, head: Option<&ObjectId>) {
    if !seq_dir(git_dir).is_dir() {
        return;
    }
    let text = match head {
        Some(oid) => format!("{oid}\n"),
        None => "\n".to_string(),
    };
    let _ = fs::write(abort_safety_path(git_dir), text);
}

/// `rollback_is_safe`: HEAD still matches the last recorded abort-safety oid.
pub fn rollback_is_safe(git_dir: &Path, actual_head: Option<&ObjectId>) -> bool {
    let expected = match fs::read_to_string(abort_safety_path(git_dir)) {
        Ok(content) => content.trim().to_string(),
        Err(_) => String::new(),
    };
    let actual = actual_head.map(|oid| oid.to_hex()).unwrap_or_default();
    let zero_is_empty = |value: &str| {
        if value.chars().all(|c| c == '0') {
            String::new()
        } else {
            value.to_string()
        }
    };
    zero_is_empty(&expected) == zero_is_empty(&actual)
}

/// Serialize the instruction sheet (`save_todo`): one `<command> <display>`
/// line per remaining item.
pub fn save_todo(git_dir: &Path, items: &[TodoItem]) -> Result<()> {
    let mut out = String::new();
    for item in items {
        out.push_str(item.action.command());
        out.push(' ');
        out.push_str(&item.display);
        out.push('\n');
    }
    fs::write(todo_path(git_dir), out)?;
    Ok(())
}

/// A parse failure for one todo line, mirroring the error cascade git prints
/// before "unusable instruction sheet".
#[derive(Debug)]
pub struct TodoParseError {
    /// The per-line errors (`invalid command '...'`, `could not parse '...'`,
    /// `missing arguments for ...`), in print order.
    pub line_errors: Vec<String>,
}

/// Result of parsing an instruction sheet for `--continue`.
pub enum TodoParse {
    /// All lines parsed; per-item resolved oids are NOT validated here (the
    /// caller resolves the display names against the repository).
    Ok(Vec<ParsedTodoLine>),
    Err(TodoParseError),
}

/// One successfully-parsed todo line: the command and its raw object-name
/// token (still to be resolved by the caller) plus the rest of the line.
#[derive(Debug, Clone)]
pub struct ParsedTodoLine {
    pub action: ReplayAction,
    pub object_name: String,
    /// The subject text following the object name (may be empty).
    pub rest: String,
}

/// Parse the instruction sheet text (mirrors `todo_list_parse_insn_buffer` +
/// `parse_insn_line` for the pick/revert subset). Comment and blank lines are
/// skipped. Lines using any *other* todo command (edit/squash/...) are parsed
/// as that command; the caller enforces the "cannot cherry-pick during a
/// revert" rule, so such lines surface here as `Other`.
pub fn parse_todo(text: &str) -> std::result::Result<Vec<ParsedTodoLine>, TodoParseError> {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    for (idx, raw_line) in text.split('\n').enumerate() {
        if raw_line.is_empty() && text.split('\n').nth(idx + 1).is_none() {
            // Trailing piece after the final newline.
            break;
        }
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let bol = line.trim_start_matches([' ', '\t']);
        if bol.is_empty() || bol.starts_with('#') {
            continue;
        }
        let mut matched = None;
        for action in [ReplayAction::Pick, ReplayAction::Revert] {
            if let Some(rest) = strip_command(bol, action.command(), action_nick(action)) {
                matched = Some((action, rest));
                break;
            }
        }
        let Some((action, rest)) = matched else {
            let token: String = bol
                .chars()
                .take_while(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
                .collect();
            errors.push(format!("invalid command '{token}'"));
            errors.push(format!("invalid line {}: {}", idx + 1, line));
            continue;
        };
        let padding = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        let rest = rest.trim_start_matches([' ', '\t']);
        if padding == 0 {
            errors.push(format!("missing arguments for {}", action.command()));
            errors.push(format!("invalid line {}: {}", idx + 1, line));
            continue;
        }
        let end = rest.find([' ', '\t']).unwrap_or(rest.len());
        let (object_name, tail) = rest.split_at(end);
        let tail = tail.trim_start_matches([' ', '\t']);
        items.push(ParsedTodoLine {
            action,
            object_name: object_name.to_string(),
            rest: tail.to_string(),
        });
    }
    if errors.is_empty() {
        Ok(items)
    } else {
        Err(TodoParseError {
            line_errors: errors,
        })
    }
}

fn action_nick(action: ReplayAction) -> Option<char> {
    match action {
        ReplayAction::Pick => Some('p'),
        ReplayAction::Revert => None,
    }
}

/// `is_command`: full command word or single-char nick, followed by a space
/// or tab; returns the remainder (starting at the separator).
fn strip_command<'a>(bol: &'a str, word: &str, nick: Option<char>) -> Option<&'a str> {
    if let Some(rest) = bol.strip_prefix(word)
        && rest.starts_with([' ', '\t'])
    {
        return Some(rest);
    }
    if let Some(nick) = nick {
        let mut chars = bol.chars();
        if chars.next() == Some(nick) {
            let rest = chars.as_str();
            if rest.starts_with([' ', '\t']) {
                return Some(rest);
            }
        }
    }
    None
}

/// Serialize replay options into `.git/sequencer/opts` (mirrors `save_opts`,
/// including key order). Creates the file only when at least one option is
/// recorded, matching git (which only writes set keys).
pub fn save_opts(git_dir: &Path, opts: &ReplayOpts) -> Result<()> {
    let mut body = String::new();
    let mut set = |key: &str, value: &str| {
        body.push_str(&format!("\t{key} = {value}\n"));
    };
    if opts.no_commit {
        set("no-commit", "true");
    }
    if let Some(edit) = opts.edit {
        set("edit", if edit { "true" } else { "false" });
    }
    if opts.allow_empty {
        set("allow-empty", "true");
    }
    if opts.allow_empty_message {
        set("allow-empty-message", "true");
    }
    if opts.drop_redundant_commits {
        set("drop-redundant-commits", "true");
    }
    if opts.keep_redundant_commits {
        set("keep-redundant-commits", "true");
    }
    if opts.signoff {
        set("signoff", "true");
    }
    if opts.record_origin {
        set("record-origin", "true");
    }
    if opts.allow_ff {
        set("allow-ff", "true");
    }
    if opts.mainline > 0 {
        let value = opts.mainline.to_string();
        set("mainline", &value);
    }
    if let Some(strategy) = &opts.strategy {
        set("strategy", strategy);
    }
    if let Some(gpg_sign) = &opts.gpg_sign {
        set("gpg-sign", gpg_sign);
    }
    for option in &opts.strategy_options {
        set("strategy-option", option);
    }
    if let Some(allow) = opts.allow_rerere_auto {
        set("allow-rerere-auto", if allow { "true" } else { "false" });
    }
    if let Some(cleanup) = &opts.default_msg_cleanup {
        set("default-msg-cleanup", cleanup);
    }
    if body.is_empty() {
        return Ok(());
    }
    fs::write(opts_path(git_dir), format!("[options]\n{body}"))?;
    Ok(())
}

/// Read `.git/sequencer/opts` back into a `ReplayOpts` (mirrors
/// `read_populate_opts` / `populate_opts_cb`). Missing file yields defaults.
pub fn read_opts(git_dir: &Path) -> Result<ReplayOpts> {
    let mut opts = ReplayOpts::default();
    let Ok(text) = fs::read_to_string(opts_path(git_dir)) else {
        return Ok(opts);
    };
    let mut in_options = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            in_options = line.eq_ignore_ascii_case("[options]");
            continue;
        }
        if !in_options {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        let truthy = value.eq_ignore_ascii_case("true") || value == "1";
        match key.as_str() {
            "no-commit" => opts.no_commit = truthy,
            "edit" => opts.edit = Some(truthy),
            "allow-empty" => opts.allow_empty = truthy,
            "allow-empty-message" => opts.allow_empty_message = truthy,
            "drop-redundant-commits" => opts.drop_redundant_commits = truthy,
            "keep-redundant-commits" => opts.keep_redundant_commits = truthy,
            "signoff" => opts.signoff = truthy,
            "record-origin" => opts.record_origin = truthy,
            "allow-ff" => opts.allow_ff = truthy,
            "mainline" => opts.mainline = value.parse().unwrap_or(0),
            "strategy" => opts.strategy = Some(value.to_string()),
            "gpg-sign" => opts.gpg_sign = Some(value.to_string()),
            "strategy-option" => opts.strategy_options.push(value.to_string()),
            "allow-rerere-auto" => opts.allow_rerere_auto = Some(truthy),
            "default-msg-cleanup" => opts.default_msg_cleanup = Some(value.to_string()),
            other => {
                return Err(GitError::Command(format!("invalid key: options.{other}")));
            }
        }
    }
    Ok(opts)
}

/// Remove the whole sequencer directory (`sequencer_remove_state`).
pub fn remove_state(git_dir: &Path) {
    let _ = fs::remove_dir_all(seq_dir(git_dir));
}

/// `have_finished_the_last_pick`: the todo sheet is missing, or holds at most
/// one line.
pub fn finished_last_pick(git_dir: &Path) -> bool {
    let Ok(buf) = fs::read_to_string(todo_path(git_dir)) else {
        return false;
    };
    match buf.find('\n') {
        None => true,
        Some(pos) => buf[pos + 1..].is_empty(),
    }
}

/// `sequencer_post_commit_cleanup`: delete `CHERRY_PICK_HEAD` /
/// `REVERT_HEAD`, and when one of them existed and the todo sheet is down to
/// its last line, drop the whole sequencer dir. Mirrors the cleanup run by
/// `git commit`, `git reset` (no pathspec) and `git checkout`.
pub fn post_commit_cleanup(git_dir: &Path) {
    let mut need_cleanup = false;
    for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD"] {
        let path = git_dir.join(name);
        if path.exists() {
            let _ = fs::remove_file(&path);
            need_cleanup = true;
        }
    }
    if need_cleanup && finished_last_pick(git_dir) {
        remove_state(git_dir);
    }
}

/// `remove_branch_state`: post-commit cleanup plus the merge scratch files.
pub fn remove_branch_state(git_dir: &Path) {
    post_commit_cleanup(git_dir);
    for name in [
        "MERGE_HEAD",
        "MERGE_RR",
        "MERGE_MSG",
        "MERGE_MODE",
        "SQUASH_MSG",
        "AUTO_MERGE",
    ] {
        let path = git_dir.join(name);
        if path.exists() {
            let _ = fs::remove_file(&path);
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

    #[test]
    fn todo_round_trips() {
        let dir = tempfile::tempdir().expect("test operation should succeed");
        let git_dir = dir.path();
        create_seq_dir(git_dir).expect("test operation should succeed");
        let items = vec![
            TodoItem {
                action: ReplayAction::Pick,
                oid: oid("21b83cd2e8f4d6d8d9615779ebaa801ba891eb04"),
                display: "21b83cd base".to_string(),
            },
            TodoItem {
                action: ReplayAction::Pick,
                oid: oid("963b36c2ba8007f62b5ae23da601530554a72537"),
                display: "963b36c picked".to_string(),
            },
        ];
        save_todo(git_dir, &items).expect("test operation should succeed");
        let text = fs::read_to_string(todo_path(git_dir)).expect("test operation should succeed");
        assert_eq!(text, "pick 21b83cd base\npick 963b36c picked\n");
        let parsed = parse_todo(&text).expect("test operation should succeed");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].object_name, "21b83cd");
        assert_eq!(parsed[0].rest, "base");
        assert_eq!(last_command(git_dir), Some(ReplayAction::Pick));
    }

    #[test]
    fn todo_parse_flags_bad_lines() {
        let err = parse_todo("pick63a subject\n").expect_err("must fail");
        assert_eq!(
            err.line_errors,
            vec![
                "invalid command 'pick63a'".to_string(),
                "invalid line 1: pick63a subject".to_string(),
            ]
        );
        // Fat-fingered extra whitespace between command and oid is fine.
        let ok = parse_todo("pick \t 21b83cd base\n").expect("test operation should succeed");
        assert_eq!(ok[0].object_name, "21b83cd");
        // Subject is optional.
        let ok = parse_todo("pick 21b83cd\n").expect("test operation should succeed");
        assert_eq!(ok[0].rest, "");
        // Missing space after the command word.
        let err = parse_todo("pick\n").expect_err("must fail");
        assert_eq!(err.line_errors.len(), 2);
    }

    #[test]
    fn opts_round_trip_matches_git_key_order() {
        let dir = tempfile::tempdir().expect("test operation should succeed");
        let git_dir = dir.path();
        create_seq_dir(git_dir).expect("test operation should succeed");
        let opts = ReplayOpts {
            signoff: true,
            mainline: 4,
            strategy: Some("recursive".to_string()),
            strategy_options: vec!["patience".to_string(), "ours".to_string()],
            edit: Some(true),
            ..ReplayOpts::default()
        };
        save_opts(git_dir, &opts).expect("test operation should succeed");
        let text = fs::read_to_string(opts_path(git_dir)).expect("test operation should succeed");
        assert_eq!(
            text,
            "[options]\n\tedit = true\n\tsignoff = true\n\tmainline = 4\n\tstrategy = recursive\n\tstrategy-option = patience\n\tstrategy-option = ours\n"
        );
        let read = read_opts(git_dir).expect("test operation should succeed");
        assert!(read.signoff);
        assert_eq!(read.mainline, 4);
        assert_eq!(read.strategy.as_deref(), Some("recursive"));
        assert_eq!(read.strategy_options, vec!["patience", "ours"]);
        assert_eq!(read.edit, Some(true));
    }

    #[test]
    fn opts_without_any_set_option_writes_no_file() {
        let dir = tempfile::tempdir().expect("test operation should succeed");
        let git_dir = dir.path();
        create_seq_dir(git_dir).expect("test operation should succeed");
        save_opts(git_dir, &ReplayOpts::default()).expect("test operation should succeed");
        assert!(!opts_path(git_dir).exists());
    }

    #[test]
    fn post_commit_cleanup_removes_state_on_last_pick() {
        let dir = tempfile::tempdir().expect("test operation should succeed");
        let git_dir = dir.path();
        create_seq_dir(git_dir).expect("test operation should succeed");
        fs::write(git_dir.join("CHERRY_PICK_HEAD"), "x\n").expect("test operation should succeed");
        fs::write(todo_path(git_dir), "pick 1234567 one\npick 89abcde two\n")
            .expect("test operation should succeed");
        post_commit_cleanup(git_dir);
        assert!(!git_dir.join("CHERRY_PICK_HEAD").exists());
        assert!(seq_dir(git_dir).is_dir(), "two items left: state stays");

        fs::write(git_dir.join("CHERRY_PICK_HEAD"), "x\n").expect("test operation should succeed");
        fs::write(todo_path(git_dir), "pick 1234567 one\n").expect("test operation should succeed");
        post_commit_cleanup(git_dir);
        assert!(!seq_dir(git_dir).is_dir(), "single item: state removed");
    }

    #[test]
    fn rollback_safety_matches_head() {
        let dir = tempfile::tempdir().expect("test operation should succeed");
        let git_dir = dir.path();
        create_seq_dir(git_dir).expect("test operation should succeed");
        let head = oid("21b83cd2e8f4d6d8d9615779ebaa801ba891eb04");
        update_abort_safety(git_dir, Some(&head));
        assert!(rollback_is_safe(git_dir, Some(&head)));
        let moved = oid("963b36c2ba8007f62b5ae23da601530554a72537");
        assert!(!rollback_is_safe(git_dir, Some(&moved)));
    }
}
