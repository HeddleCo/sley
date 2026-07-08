//! `git add --interactive` (`add -i`) and `git add --patch` (`add -p`).
//!
//! This is a faithful port of upstream git's `add-interactive.c` (the main
//! menu loop, `list_and_choose`, and the status table) and `add-patch.c` (the
//! per-hunk decision REPL). The data-producing operations — generating the
//! worktree/index/HEAD diffs, staging files, reverting the index, and applying
//! a selected hunk to the index — are delegated to sley's own already-parity
//! subcommands invoked as subprocesses, exactly as git itself spawns
//! `git diff-files` / `git apply --cached` from these helpers. The logic in
//! this module is therefore confined to the REPL and the byte-exact prompt /
//! table formatting that the upstream t3701 oracle pins down.

use std::env;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use sley::plumbing::{sley_config};

use sley::GitConfig;
use sley::{GitError, Result};

use crate::{worktree_root_for_git_dir};

/// Resolve the path to the running sley binary so the engine can re-invoke
/// data-producing subcommands (diff, add, reset, apply) the same way git
/// spawns `git diff-files` from add-patch.c.
fn self_bin() -> PathBuf {
    env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"))
}

/// Run a sley subcommand, capturing stdout. The child inherits the cwd and
/// environment (so config/identity resolve identically). Returns the raw
/// stdout bytes; stderr is inherited (matches git, which lets child stderr
/// flow to the terminal).
fn run_capture(args: &[&str]) -> io::Result<Vec<u8>> {
    let out = Command::new(self_bin())
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()?;
    Ok(out.stdout)
}

/// Run a sley subcommand for side effects, feeding `stdin_bytes` if any.
/// Returns the exit status code (0 on success).
fn run_status(args: &[&str], stdin_bytes: Option<&[u8]>) -> io::Result<i32> {
    let mut child = Command::new(self_bin())
        .args(args)
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(bytes) = stdin_bytes
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(bytes);
    }
    let status = child.wait()?;
    Ok(status.code().unwrap_or(1))
}

#[derive(Clone)]
struct InteractiveStyle {
    enabled: bool,
    header: String,
    help: String,
    prompt: String,
    error: String,
    reset: &'static str,
}

impl InteractiveStyle {
    fn load(git_dir: &Path) -> Self {
        let config = crate::read_repo_config(git_dir).ok();
        let get = |section: &str, subsection: Option<&str>, key: &str| -> Option<String> {
            config
                .as_ref()
                .and_then(|c| c.get(section, subsection, key))
                .map(|v| v.trim().to_string())
        };
        let interactive = get("color", None, "interactive");
        let color_ui = get("color", None, "ui");
        let enabled = match interactive
            .as_deref()
            .and_then(sley_config::parse_config_bool)
        {
            Some(value) => value,
            None => match color_ui.as_deref().and_then(sley_config::parse_config_bool) {
                Some(value) => value,
                None => {
                    env::var("GIT_PAGER_IN_USE").is_ok()
                        && env::var("TERM").map(|term| term != "dumb").unwrap_or(false)
                }
            },
        };
        let color_slot = |slot: &str, default: &str| -> String {
            if !enabled {
                String::new()
            } else {
                get("color", Some("interactive"), slot)
                    .as_deref()
                    .and_then(ansi_color)
                    .unwrap_or(default)
                    .to_string()
            }
        };
        InteractiveStyle {
            enabled,
            header: color_slot("header", "\x1b[1m"),
            help: color_slot("help", "\x1b[1;31m"),
            prompt: color_slot("prompt", "\x1b[1;34m"),
            error: color_slot("error", "\x1b[1;31m"),
            reset: "\x1b[m",
        }
    }

    fn stdout_line(&self, color: &str, text: &str) {
        if self.enabled {
            println!("{color}{text}{}", self.reset);
        } else {
            println!("{text}");
        }
    }

    fn stderr_line(&self, color: &str, text: &str) {
        if self.enabled {
            eprintln!("{color}{text}{}", self.reset);
        } else {
            eprintln!("{text}");
        }
    }

    fn prompt(&self, text: &str) {
        if self.enabled {
            print!("{}{text}{}> ", self.prompt, self.reset);
        } else {
            print!("{text}> ");
        }
    }
}

fn ansi_color(value: &str) -> Option<&'static str> {
    let mut bold = false;
    let mut color = None;
    for word in value.split_whitespace() {
        match word.to_ascii_lowercase().as_str() {
            "bold" => bold = true,
            "black" => color = Some(30),
            "red" => color = Some(31),
            "green" => color = Some(32),
            "yellow" => color = Some(33),
            "blue" => color = Some(34),
            "magenta" => color = Some(35),
            "cyan" => color = Some(36),
            "white" => color = Some(37),
            _ => {}
        }
    }
    match (bold, color) {
        (true, Some(31)) => Some("\x1b[1;31m"),
        (true, Some(34)) => Some("\x1b[1;34m"),
        (true, _) => Some("\x1b[1m"),
        (false, Some(30)) => Some("\x1b[30m"),
        (false, Some(31)) => Some("\x1b[31m"),
        (false, Some(32)) => Some("\x1b[32m"),
        (false, Some(33)) => Some("\x1b[33m"),
        (false, Some(34)) => Some("\x1b[34m"),
        (false, Some(35)) => Some("\x1b[35m"),
        (false, Some(36)) => Some("\x1b[36m"),
        (false, Some(37)) => Some("\x1b[37m"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// File status table (the `staged unstaged path` listing).
// ---------------------------------------------------------------------------

/// One modified/untracked path with its add/del counts on each side.
#[derive(Clone)]
struct FileItem {
    path: String,
    /// `Some((add, del))` when the side has changes; `None` when unchanged.
    index: Option<(usize, usize)>,
    worktree: Option<(usize, usize)>,
    index_binary: bool,
    worktree_binary: bool,
    prefix_len: usize,
}

/// Parse `git diff --numstat` output into a map path -> (add, del, binary).
/// Binary files emit `-\t-\t<path>`.
fn parse_numstat(bytes: &[u8]) -> Vec<(String, usize, usize, bool)> {
    let mut out = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(line);
        let mut parts = text.splitn(3, '\t');
        let a = parts.next().unwrap_or("");
        let d = parts.next().unwrap_or("");
        let p = match parts.next() {
            Some(p) => p.to_string(),
            None => continue,
        };
        if a == "-" || d == "-" {
            out.push((p, 0, 0, true));
        } else {
            let add = a.parse().unwrap_or(0);
            let del = d.parse().unwrap_or(0);
            out.push((p, add, del, false));
        }
    }
    out
}

/// Which side(s) of the index/worktree to collect for a listing.
#[derive(Clone, Copy, PartialEq)]
enum Filter {
    NoFilter,
    WorktreeOnly,
    IndexOnly,
}

/// Build the modified-files list, mirroring git's `get_modified_files`:
/// staged side = `diff-index --cached HEAD`, unstaged side = `diff-files`.
fn get_modified_files(filter: Filter, paths: &[String]) -> Vec<FileItem> {
    let mut index_map: Vec<(String, usize, usize, bool)> = Vec::new();
    let mut worktree_map: Vec<(String, usize, usize, bool)> = Vec::new();

    if filter != Filter::WorktreeOnly {
        let mut args = vec!["diff", "--cached", "--numstat"];
        if !paths.is_empty() {
            args.push("--");
        }
        for p in paths {
            args.push(p.as_str());
        }
        if let Ok(out) = run_capture(&args) {
            index_map = parse_numstat(&out);
        }
    }
    if filter != Filter::IndexOnly {
        let mut args = vec!["diff", "--numstat"];
        if !paths.is_empty() {
            args.push("--");
        }
        for p in paths {
            args.push(p.as_str());
        }
        if let Ok(out) = run_capture(&args) {
            worktree_map = parse_numstat(&out);
        }
    }

    // Merge by path, sorted like git (string_list is sorted).
    let mut paths_set: Vec<String> = Vec::new();
    for (p, ..) in index_map.iter().chain(worktree_map.iter()) {
        if !paths_set.contains(p) {
            paths_set.push(p.clone());
        }
    }
    paths_set.sort();

    let mut items = Vec::new();
    for p in paths_set {
        let idx = index_map.iter().find(|e| e.0 == p);
        let wt = worktree_map.iter().find(|e| e.0 == p);
        items.push(FileItem {
            path: p,
            index: idx.map(|e| (e.1, e.2)),
            worktree: wt.map(|e| (e.1, e.2)),
            index_binary: idx.map(|e| e.3).unwrap_or(false),
            worktree_binary: wt.map(|e| e.3).unwrap_or(false),
            prefix_len: 0,
        });
    }
    items
}

/// Render an add/del cell: `binary`, `+A/-D`, or the no-change placeholder.
fn render_adddel(side: Option<(usize, usize)>, binary: bool, no_changes: &str) -> String {
    if binary {
        "binary".to_string()
    } else if let Some((add, del)) = side {
        format!("+{add}/-{del}")
    } else {
        no_changes.to_string()
    }
}

/// `is_valid_prefix` from add-interactive.c: a unique prefix may not collide
/// with `list_and_choose`'s reserved tokens.
fn is_valid_prefix(s: &str, prefix_len: usize) -> bool {
    if prefix_len == 0 {
        return false;
    }
    let bytes = s.as_bytes();
    // No separators within the prefix.
    if bytes[..prefix_len]
        .iter()
        .any(|&b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b','))
    {
        return false;
    }
    let first = bytes[0];
    if first == b'-' || first.is_ascii_digit() {
        return false;
    }
    if prefix_len == 1 && (first == b'*' || first == b'?') {
        return false;
    }
    true
}

/// Compute unique-prefix lengths over the item path list, like
/// `find_unique_prefixes`. A path's prefix is the shortest leading substring
/// that no other path shares (capped at the full name).
fn find_unique_prefixes(items: &mut [FileItem]) {
    let names: Vec<String> = items.iter().map(|i| i.path.clone()).collect();
    for (i, item) in items.iter_mut().enumerate() {
        let name = &names[i];
        let mut len = 0;
        for cand in 1..=name.len() {
            // Respect char boundaries.
            if !name.is_char_boundary(cand) {
                continue;
            }
            let prefix = &name[..cand];
            let unique = names
                .iter()
                .enumerate()
                .all(|(j, other)| j == i || !other.starts_with(prefix));
            if unique {
                len = cand;
                break;
            }
        }
        item.prefix_len = len;
    }
}

/// Format one file row. With `only_names` (add-untracked), prints just the
/// name; otherwise the `staged unstaged path` columns.
fn format_file_row(
    item: &FileItem,
    idx: usize,
    selected: bool,
    only_names: bool,
    style: &InteractiveStyle,
) -> String {
    let highlighted = if item.prefix_len > 0 && is_valid_prefix(&item.path, item.prefix_len) {
        if style.enabled {
            format!(
                "{}{}{}{}",
                style.prompt,
                &item.path[..item.prefix_len],
                style.reset,
                &item.path[item.prefix_len..]
            )
        } else {
            format!(
                "[{}]{}",
                &item.path[..item.prefix_len],
                &item.path[item.prefix_len..]
            )
        }
    } else {
        item.path.clone()
    };
    let mark = if selected { '*' } else { ' ' };
    if only_names {
        return format!("{mark}{:2}: {highlighted}", idx + 1);
    }
    let index = render_adddel(item.index, item.index_binary, "unchanged");
    let worktree = render_adddel(item.worktree, item.worktree_binary, "nothing");
    // modified_fmt = "%12s %12s %s"
    format!(
        "{mark}{:2}: {:>12} {:>12} {highlighted}",
        idx + 1,
        index,
        worktree
    )
}

/// The status header line ("           staged     unstaged path").
fn status_header() -> String {
    // modified_fmt applied to ("staged","unstaged","path") with a leading
    // two-space + ": " offset removed; git prints the header WITHOUT the
    // `%c%2d: ` prefix, so it is `"%12s %12s %s"` of the three words plus the
    // leading spaces that align under the row's `   N: ` gutter.
    format!("{:>12} {:>12} {}", "staged", "unstaged", "path")
}

/// Print the listing (header + rows). The header is indented to line up with
/// the row gutter `%c%2d: ` which is 5 columns wide.
fn print_list(
    items: &[FileItem],
    selected: &[bool],
    only_names: bool,
    header: bool,
    style: &InteractiveStyle,
) {
    if items.is_empty() {
        return;
    }
    if header {
        // git prints the header with a 5-char gutter ("%c%2d: " width) before
        // the `%12s %12s %s` columns: 5 spaces + "      staged ..." yields the
        // observed `           staged     unstaged path` (11 leading spaces).
        let line = format!("     {}", status_header());
        style.stdout_line(&style.header, &line);
    }
    for (i, item) in items.iter().enumerate() {
        let sel = selected.get(i).copied().unwrap_or(false);
        println!("{}", format_file_row(item, i, sel, only_names, style));
    }
}

// ---------------------------------------------------------------------------
// list_and_choose
// ---------------------------------------------------------------------------

/// Read one line from stdin; returns None on EOF.
fn read_line(stdin: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => {
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            Some(line)
        }
        Err(_) => None,
    }
}

/// Parse a selection token list ("2-5 8-", "*", "1", "-3") into selected flags
/// toggled over `selected`. Returns the number of newly-selected entries seen
/// in this call (git returns the running count of selected items).
fn apply_choices(input: &str, selected: &mut [bool], items: &[FileItem]) -> Option<String> {
    let n = selected.len();
    for raw in input.split([' ', '\t', '\r', '\n', ',']) {
        if raw.is_empty() {
            continue;
        }
        let (choose, tok) = if let Some(rest) = raw.strip_prefix('-') {
            (false, rest)
        } else {
            (true, raw)
        };
        if tok.is_empty() {
            continue;
        }
        // Non-numeric, non-wildcard tokens are unique-prefix name selections
        // (e.g. `t` for "to-delete"). Match against the item paths by prefix.
        let first = tok.as_bytes()[0];
        if tok != "*" && !first.is_ascii_digit() {
            let mut matches = items
                .iter()
                .enumerate()
                .filter(|(_, it)| it.path.starts_with(tok))
                .map(|(idx, _)| idx);
            if let Some(idx) = matches.next()
                && matches.next().is_none()
                && idx < n
            {
                selected[idx] = choose;
            } else {
                return Some(tok.to_string());
            }
            continue;
        }
        let (from, to): (isize, isize) = if tok == "*" {
            (0, n as isize)
        } else if let Some(dash) = tok.find('-') {
            let lo = tok[..dash].parse::<isize>().ok();
            let hi_str = &tok[dash + 1..];
            let lo = match lo {
                Some(v) => v - 1,
                None => return Some(tok.to_string()),
            };
            let hi = if hi_str.is_empty() {
                n as isize
            } else {
                match hi_str.parse::<isize>() {
                    Ok(v) => v,
                    Err(_) => return Some(tok.to_string()),
                }
            };
            (lo, hi)
        } else if let Ok(v) = tok.parse::<isize>() {
            (v - 1, v)
        } else {
            return Some(tok.to_string());
        };
        if from < 0 || from >= n as isize {
            return Some(tok.to_string());
        }
        let lo = from.max(0) as usize;
        let hi = (to.max(0) as usize).min(n);
        for s in selected.iter_mut().take(hi).skip(lo) {
            *s = choose;
        }
    }
    None
}

/// Interactive multi-select over a file list. Returns the indices selected, or
/// None on EOF/empty (quit). `prompt` is e.g. "Update", "Revert".
fn list_and_choose(
    stdin: &mut impl BufRead,
    items: &mut [FileItem],
    prompt: &str,
    only_names: bool,
    immediate: bool,
    style: &InteractiveStyle,
) -> Option<Vec<usize>> {
    find_unique_prefixes(items);
    let mut selected = vec![false; items.len()];
    loop {
        print_list(items, &selected, only_names, false, style);
        if style.enabled {
            print!("{}{prompt}{}>> ", style.prompt, style.reset);
        } else {
            print!("{prompt}>> ");
        }
        let _ = io::stdout().flush();
        let line = match read_line(stdin) {
            Some(l) => l,
            None => {
                println!();
                return if immediate { None } else { collect(&selected) };
            }
        };
        if line.is_empty() {
            break;
        }
        if line == "?" {
            print_choose_help();
            continue;
        }
        if let Some(bad) = apply_choices(&line, &mut selected, items) {
            style.stderr_line(&style.error, &format!("Huh ({bad})?"));
        }
        if immediate {
            return collect(&selected);
        }
    }
    collect(&selected)
}

fn collect(selected: &[bool]) -> Option<Vec<usize>> {
    let v: Vec<usize> = selected
        .iter()
        .enumerate()
        .filter_map(|(i, &s)| if s { Some(i) } else { None })
        .collect();
    Some(v)
}

fn print_choose_help() {
    print!(
        "Prompt help:\n\
         1          - select a single item\n\
         3-5        - select a range of items\n\
         2-3,6-9    - select multiple ranges\n\
         foo        - select item based on unique prefix\n\
         -...       - unselect specified items\n\
         *          - choose all items\n\
                    - (empty) finish selecting\n"
    );
}

// ---------------------------------------------------------------------------
// Main menu (add -i)
// ---------------------------------------------------------------------------

fn print_menu(style: &InteractiveStyle) {
    style.stdout_line(&style.header, "*** Commands ***");
    if style.enabled {
        println!(
            "  1: {}s{}tatus\t  2: {}u{}pdate\t  3: {}r{}evert\t  4: {}a{}dd untracked",
            style.prompt,
            style.reset,
            style.prompt,
            style.reset,
            style.prompt,
            style.reset,
            style.prompt,
            style.reset
        );
        println!(
            "  5: {}p{}atch\t  6: {}d{}iff\t  7: {}q{}uit\t  8: {}h{}elp",
            style.prompt,
            style.reset,
            style.prompt,
            style.reset,
            style.prompt,
            style.reset,
            style.prompt,
            style.reset
        );
    } else {
        println!("  1: [s]tatus\t  2: [u]pdate\t  3: [r]evert\t  4: [a]dd untracked");
        println!("  5: [p]atch\t  6: [d]iff\t  7: [q]uit\t  8: [h]elp");
    }
}

fn print_status_table(paths: &[String], style: &InteractiveStyle) {
    let items = get_modified_files(Filter::NoFilter, paths);
    let selected = vec![false; items.len()];
    print_list(&items, &selected, false, true, style);
    println!();
}

fn run_main_loop(paths: &[String], style: &InteractiveStyle) -> Result<()> {
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    print_status_table(paths, style);
    loop {
        print_menu(style);
        style.prompt("What now");
        let _ = io::stdout().flush();
        let line = match read_line(&mut handle) {
            Some(l) => l,
            None => {
                println!();
                println!("Bye.");
                return Ok(());
            }
        };
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        match menu_command(cmd) {
            Some(MenuCmd::Status) => print_status_table(paths, style),
            Some(MenuCmd::Update) => run_update(&mut handle, paths, style)?,
            Some(MenuCmd::Revert) => run_revert(&mut handle, paths, style)?,
            Some(MenuCmd::AddUntracked) => run_add_untracked(&mut handle, paths, style)?,
            Some(MenuCmd::Patch) => run_patch_menu(&mut handle, paths, style)?,
            Some(MenuCmd::Diff) => run_diff(&mut handle, paths, style)?,
            Some(MenuCmd::Quit) => {
                println!("Bye.");
                return Ok(());
            }
            Some(MenuCmd::Help) => print_main_help(style),
            None => {
                let first = cmd
                    .split([' ', '\t', '\r', '\n', ','])
                    .next()
                    .unwrap_or(cmd);
                style.stderr_line(&style.error, &format!("Huh ({first})?"));
            }
        }
    }
}

enum MenuCmd {
    Status,
    Update,
    Revert,
    AddUntracked,
    Patch,
    Diff,
    Quit,
    Help,
}

fn menu_command(cmd: &str) -> Option<MenuCmd> {
    // Accept the leading letter or the number.
    match cmd {
        "1" | "s" | "status" => Some(MenuCmd::Status),
        "2" | "u" | "update" => Some(MenuCmd::Update),
        "3" | "r" | "revert" => Some(MenuCmd::Revert),
        "4" | "a" | "add untracked" => Some(MenuCmd::AddUntracked),
        "5" | "p" | "patch" => Some(MenuCmd::Patch),
        "6" | "d" | "diff" => Some(MenuCmd::Diff),
        "7" | "q" | "quit" => Some(MenuCmd::Quit),
        "8" | "h" | "help" => Some(MenuCmd::Help),
        _ => None,
    }
}

fn print_main_help(style: &InteractiveStyle) {
    for line in [
        "status        - show paths with changes",
        "update        - add working tree state to the staged set of changes",
        "revert        - revert staged set of changes back to the HEAD version",
        "patch         - pick hunks and update selectively",
        "diff          - view diff between HEAD and index",
        "add untracked - add contents of untracked files to the staged set of changes",
    ] {
        style.stdout_line(&style.help, line);
    }
}

fn run_update(stdin: &mut impl BufRead, paths: &[String], style: &InteractiveStyle) -> Result<()> {
    let mut items = get_modified_files(Filter::WorktreeOnly, paths);
    if items.is_empty() {
        println!();
        return Ok(());
    }
    let chosen = list_and_choose(stdin, &mut items, "Update", false, false, style);
    let chosen = match chosen {
        Some(v) if !v.is_empty() => v,
        _ => {
            println!();
            return Ok(());
        }
    };
    // Stage each chosen path: `add -- <path>` (handles deletions too).
    let mut count = 0;
    for &i in &chosen {
        let p = &items[i].path;
        let _ = run_status(&["add", "--", p.as_str()], None);
        count += 1;
    }
    if count == 1 {
        println!("updated 1 path");
    } else {
        println!("updated {count} paths");
    }
    println!();
    Ok(())
}

fn run_revert(stdin: &mut impl BufRead, paths: &[String], style: &InteractiveStyle) -> Result<()> {
    let mut items = get_modified_files(Filter::IndexOnly, paths);
    if items.is_empty() {
        println!();
        return Ok(());
    }
    let chosen = list_and_choose(stdin, &mut items, "Revert", false, false, style);
    let chosen = match chosen {
        Some(v) if !v.is_empty() => v,
        _ => {
            println!();
            return Ok(());
        }
    };
    let mut count = 0;
    // Reset selected paths in the index back to the HEAD version (or remove them
    // when HEAD is unborn / the path is not in HEAD). `reset -q -- <path>` resets
    // against HEAD-or-empty-tree, matching add-interactive.c's run_revert which
    // diffs the index against HEAD (empty tree when initial).
    for &i in &chosen {
        let p = &items[i].path;
        let _ = run_status(&["reset", "-q", "--", p.as_str()], None);
        count += 1;
    }
    if count == 1 {
        println!("reverted 1 path");
    } else {
        println!("reverted {count} paths");
    }
    println!();
    Ok(())
}

fn run_add_untracked(
    stdin: &mut impl BufRead,
    paths: &[String],
    style: &InteractiveStyle,
) -> Result<()> {
    let mut items = get_untracked_files(paths);
    if items.is_empty() {
        println!("No untracked files.");
        println!();
        return Ok(());
    }
    let chosen = list_and_choose(stdin, &mut items, "Add untracked", true, false, style);
    let chosen = match chosen {
        Some(v) if !v.is_empty() => v,
        _ => {
            println!();
            return Ok(());
        }
    };
    let mut count = 0;
    for &i in &chosen {
        let p = &items[i].path;
        let _ = run_status(&["add", "--", p.as_str()], None);
        count += 1;
    }
    if count == 1 {
        println!("added 1 path");
    } else {
        println!("added {count} paths");
    }
    println!();
    Ok(())
}

fn get_untracked_files(paths: &[String]) -> Vec<FileItem> {
    // `git ls-files --others --exclude-standard` over the pathspec.
    let mut args = vec!["ls-files", "--others", "--exclude-standard"];
    if !paths.is_empty() {
        args.push("--");
    }
    for p in paths {
        args.push(p.as_str());
    }
    let out = run_capture(&args).unwrap_or_default();
    let mut items = Vec::new();
    for line in out.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        items.push(FileItem {
            path: String::from_utf8_lossy(line).into_owned(),
            index: None,
            worktree: None,
            index_binary: false,
            worktree_binary: false,
            prefix_len: 0,
        });
    }
    items
}

fn run_diff(stdin: &mut impl BufRead, paths: &[String], style: &InteractiveStyle) -> Result<()> {
    let mut items = get_modified_files(Filter::IndexOnly, paths);
    if items.is_empty() {
        println!();
        return Ok(());
    }
    let chosen = list_and_choose(stdin, &mut items, "Review diff", false, true, style);
    if let Some(v) = chosen
        && !v.is_empty()
    {
        let mut args = vec!["diff".to_string(), "--cached".to_string(), "--".to_string()];
        for &i in &v {
            args.push(items[i].path.clone());
        }
        let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_capture(&argrefs).unwrap_or_default();
        io::stdout().write_all(&out).ok();
    }
    println!();
    Ok(())
}

fn run_patch_menu(
    stdin: &mut impl BufRead,
    paths: &[String],
    style: &InteractiveStyle,
) -> Result<()> {
    let mut items = get_modified_files(Filter::WorktreeOnly, paths);
    // Drop binary / unmerged entries (best-effort: numstat marks binary).
    items.retain(|i| !i.index_binary && !i.worktree_binary);
    if items.is_empty() {
        eprintln!("No changes.");
        return Ok(());
    }
    let chosen = list_and_choose(stdin, &mut items, "Patch update", false, false, style);
    let chosen = match chosen {
        Some(v) if !v.is_empty() => v,
        _ => return Ok(()),
    };
    let selected_paths: Vec<String> = chosen.iter().map(|&i| items[i].path.clone()).collect();
    let git_dir = crate::session::cli_git_dir()?;
    let cfg = resolve_patch_config(&git_dir, None, None, true)?;
    super::add_patch::run_add_patch(
        super::add_patch::PatchMode::Add,
        &selected_paths,
        None,
        stdin,
        cfg,
    )
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// `git add --interactive` / `git add -i [-- <pathspec>...]`.
pub(crate) fn cmd_add_interactive(paths: &[String]) -> Result<()> {
    // Ensure we are inside a repo (git errors otherwise via discover).
    let git_dir = crate::session::cli_git_dir()?;
    let _ = worktree_root_for_git_dir(&git_dir)?;
    let style = InteractiveStyle::load(&git_dir);
    run_main_loop(paths, &style)
}

/// `git add --patch [-U<n>] [--inter-hunk-context=<n>] [-- <pathspec>...]`.
///
/// `context` / `interhunk` carry an explicit `-U` / `--inter-hunk-context` from
/// add's own argv (`None` → fall back to config). They mirror `opts->context` /
/// `opts->interhunkcontext` in add-patch.c, which override the config values.
pub(crate) fn cmd_add_patch(
    paths: &[String],
    context: Option<i64>,
    interhunk: Option<i64>,
    auto_advance: bool,
) -> Result<()> {
    let git_dir = crate::session::cli_git_dir()?;
    let _ = worktree_root_for_git_dir(&git_dir)?;
    let cfg = resolve_patch_config(&git_dir, context, interhunk, auto_advance)?;
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    super::add_patch::run_add_patch(
        super::add_patch::PatchMode::Add,
        paths,
        None,
        &mut handle,
        cfg,
    )
}

pub(crate) fn cmd_stash_patch(
    paths: &[String],
    context: Option<i64>,
    interhunk: Option<i64>,
    auto_advance: bool,
    quiet: bool,
) -> Result<bool> {
    let git_dir = crate::session::cli_git_dir()?;
    let _ = worktree_root_for_git_dir(&git_dir)?;
    let cfg = resolve_patch_config(&git_dir, context, interhunk, auto_advance)?;
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    super::add_patch::run_stash_patch(paths, None, &mut handle, cfg, quiet)
}

/// Resolve the diff-tuning [`PatchConfig`] the way add-patch.c's
/// `init_interactive_config` does: read `diff.context` / `diff.interHunkContext`
/// / `diff.algorithm` config (rejecting negative context), then let an explicit
/// `-U` / `--inter-hunk-context` from the command line override (also rejecting
/// negatives). The die messages match git byte-for-byte (t3701 #88/#90/#91).
pub(crate) fn resolve_patch_config(
    git_dir: &std::path::Path,
    cli_context: Option<i64>,
    cli_interhunk: Option<i64>,
    auto_advance: bool,
) -> Result<super::add_patch::PatchConfig> {
    let config = crate::read_repo_config(git_dir).ok();
    let read_int = |key: &str| -> Option<i64> {
        config
            .as_ref()
            .and_then(|c| c.get("diff", None, key))
            .and_then(|v| v.trim().parse::<i64>().ok())
    };
    // diff.context (config), validated non-negative.
    let mut context = read_int("context");
    if let Some(value) = context
        && value < 0
    {
        eprintln!("fatal: diff.context cannot be negative");
        return Err(GitError::Exit(128));
    }
    let mut interhunk = read_int("interHunkContext");
    if let Some(value) = interhunk
        && value < 0
    {
        eprintln!("fatal: diff.interHunkContext cannot be negative");
        return Err(GitError::Exit(128));
    }
    // Command-line `-U` / `--inter-hunk-context` override the config, validated.
    if let Some(value) = cli_context {
        if value < 0 {
            eprintln!("fatal: --unified cannot be negative");
            return Err(GitError::Exit(128));
        }
        context = Some(value);
    }
    if let Some(value) = cli_interhunk {
        if value < 0 {
            eprintln!("fatal: --inter-hunk-context cannot be negative");
            return Err(GitError::Exit(128));
        }
        interhunk = Some(value);
    }
    let diff_algorithm = config
        .as_ref()
        .and_then(|c| c.get("diff", None, "algorithm"))
        .map(|v| v.trim().to_string());
    let colors = patch_color_enabled(config.as_ref(), "diff")
        .then(|| super::diff_words::DiffColors::enabled(config.as_ref()));
    let interactive_enabled = patch_color_enabled(config.as_ref(), "interactive");
    let prompt_color = patch_color_slot(
        config.as_ref(),
        interactive_enabled,
        "interactive",
        "prompt",
        "\x1b[1;34m",
    );
    let header_color = patch_color_slot(
        config.as_ref(),
        interactive_enabled,
        "interactive",
        "header",
        "\x1b[1m",
    );
    let reset_interactive = if interactive_enabled {
        "\x1b[m".to_string()
    } else {
        String::new()
    };
    let diff_filter = colors.as_ref().and_then(|_| {
        config
            .as_ref()
            .and_then(|c| c.get("interactive", None, "diffFilter"))
            .map(str::to_string)
    });
    Ok(super::add_patch::PatchConfig {
        auto_advance,
        context: context.map(|v| v as usize),
        interhunk: interhunk.map(|v| v as usize),
        diff_algorithm,
        colors,
        prompt_color,
        header_color,
        reset_interactive,
        diff_filter,
    })
}

fn patch_color_enabled(config: Option<&GitConfig>, slot: &str) -> bool {
    let key = config
        .and_then(|c| c.get("color", None, slot))
        .or_else(|| config.and_then(|c| c.get("color", None, "ui")));
    match key.as_deref().map(str::trim) {
        Some("always") | Some("auto") => true,
        Some(value) => sley_config::parse_config_bool(value).unwrap_or(false),
        None => {
            env::var("GIT_PAGER_IN_USE").is_ok()
                && env::var("TERM").is_ok_and(|term| term != "dumb")
        }
    }
}

fn patch_color_slot(
    config: Option<&GitConfig>,
    enabled: bool,
    section: &str,
    slot: &str,
    default: &str,
) -> String {
    if !enabled {
        return String::new();
    }
    config
        .and_then(|c| c.get("color", Some(section), slot))
        .and_then(|value| super::diff_words::parse_color_value(&value))
        .unwrap_or_else(|| default.to_string())
}

/// Drain all of stdin (used when a command path needs the buffered input but
/// no longer wants it). Currently unused placeholder for future modes.
#[allow(dead_code)]
fn drain_stdin() {
    let mut buf = Vec::new();
    let _ = io::stdin().read_to_end(&mut buf);
}
