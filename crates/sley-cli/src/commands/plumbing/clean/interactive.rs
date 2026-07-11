//! The terminal state machine for `clean --interactive`.
//!
//! Candidate discovery and removal stay in the clean command/worktree engine;
//! this module only transforms an already validated candidate list from user
//! input. That keeps prompt grammar out of filesystem policy while making the
//! interaction independently testable.

use super::CleanTarget;
use crate::sley_worktree;
use sley::Result;
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};

const COMMANDS: &[(char, &str)] = &[
    ('c', "clean"),
    ('f', "filter by pattern"),
    ('s', "select by numbers"),
    ('a', "ask each"),
    ('q', "quit"),
    ('h', "help"),
];

pub(super) fn select_clean_targets(targets: Vec<CleanTarget>) -> Result<Vec<CleanTarget>> {
    if targets.is_empty() {
        return Ok(targets);
    }

    let color = io::stdout().is_terminal();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    interactive_loop(targets, &mut input, &mut output, color)
}

fn interactive_loop<R: BufRead, W: Write>(
    mut targets: Vec<CleanTarget>,
    input: &mut R,
    output: &mut W,
    color: bool,
) -> Result<Vec<CleanTarget>> {
    while !targets.is_empty() {
        write_header(output, targets.len(), color)?;
        write_target_list(output, &targets)?;
        write_command_menu(output)?;
        write!(output, "What now> ")?;
        output.flush()?;

        let Some(choice) = read_line(input)? else {
            writeln!(output, "Bye.")?;
            return Ok(Vec::new());
        };
        let Some(command) = unique_command(&choice) else {
            if !choice.is_empty() {
                writeln!(output, "Huh ({choice})?")?;
            }
            continue;
        };

        match command {
            'c' => return Ok(targets),
            'f' => filter_by_patterns(&mut targets, input, output)?,
            's' => {
                targets = select_by_numbers(targets, input, output)?;
            }
            'a' => return ask_each(targets, input, output),
            'q' => {
                writeln!(output, "Bye.")?;
                return Ok(Vec::new());
            }
            'h' => write_help(output)?,
            _ => unreachable!("command table and dispatch must agree"),
        }
    }

    writeln!(output, "No more files to clean, exiting.")?;
    Ok(Vec::new())
}

fn write_header<W: Write>(output: &mut W, count: usize, color: bool) -> io::Result<()> {
    if color {
        write!(output, "\x1b[1m")?;
    }
    if count == 1 {
        write!(output, "Would remove the following item:")?;
    } else {
        write!(output, "Would remove the following items:")?;
    }
    if color {
        write!(output, "\x1b[m")?;
    }
    writeln!(output)
}

fn write_target_list<W: Write>(output: &mut W, targets: &[CleanTarget]) -> io::Result<()> {
    let items = targets
        .iter()
        .map(|target| String::from_utf8_lossy(&target.display).into_owned())
        .collect::<Vec<_>>();
    write_columns(output, &items, true)
}

fn write_command_menu<W: Write>(output: &mut W) -> io::Result<()> {
    writeln!(output, "*** Commands ***")?;
    writeln!(
        output,
        "    1: clean                2: filter by pattern    3: select by numbers"
    )?;
    writeln!(
        output,
        "    4: ask each             5: quit                 6: help"
    )
}

fn write_help<W: Write>(output: &mut W) -> io::Result<()> {
    writeln!(
        output,
        "clean               - start cleaning\n\
         filter by pattern   - exclude items from deletion\n\
         select by numbers   - select items to be deleted by numbers\n\
         ask each            - confirm each deletion (like \"rm -i\")\n\
         quit                - stop cleaning\n\
         help                - this screen\n\
         ?                   - help for prompt selection"
    )
}

fn read_line<R: BufRead>(input: &mut R) -> io::Result<Option<String>> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

fn unique_command(input: &str) -> Option<char> {
    let input = input.trim();
    if input == "?" {
        return Some('h');
    }
    if let Ok(number) = input.parse::<usize>()
        && let Some((hotkey, _)) = COMMANDS.get(number.checked_sub(1)?)
    {
        return Some(*hotkey);
    }
    if input.len() == 1 {
        let needle = input.chars().next()?.to_ascii_lowercase();
        if COMMANDS.iter().any(|(hotkey, _)| *hotkey == needle) {
            return Some(needle);
        }
    }
    let input = input.to_ascii_lowercase();
    let mut matches = COMMANDS
        .iter()
        .filter(|(_, title)| title.starts_with(&input));
    let first = matches.next()?;
    matches.next().is_none().then_some(first.0)
}

fn filter_by_patterns<R: BufRead, W: Write>(
    targets: &mut Vec<CleanTarget>,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    loop {
        if targets.is_empty() {
            return Ok(());
        }
        write_target_list(output, targets)?;
        write!(output, "Input ignore patterns>> ")?;
        output.flush()?;
        let Some(line) = read_line(input)? else {
            writeln!(output)?;
            return Ok(());
        };
        if line.is_empty() {
            return Ok(());
        }
        let patterns = line
            .split_ascii_whitespace()
            .filter(|pattern| !pattern.is_empty())
            .collect::<Vec<_>>();
        let before = targets.len();
        targets.retain(|target| !patterns_exclude(&target.display, &patterns));
        if targets.len() == before {
            writeln!(output, "WARNING: Cannot find items matched by: {line}")?;
        }
    }
}

fn patterns_exclude(path: &[u8], patterns: &[&str]) -> bool {
    let path = path.strip_suffix(b"/").unwrap_or(path);
    let basename = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
    let mut excluded = false;
    for raw in patterns {
        let (negative, pattern) = raw
            .strip_prefix('!')
            .map_or((false, *raw), |pattern| (true, pattern));
        if pattern.is_empty() {
            continue;
        }
        let subject = if pattern.as_bytes().contains(&b'/') {
            path
        } else {
            basename
        };
        if sley_worktree::wildmatch(pattern.as_bytes(), subject, 0) {
            excluded = !negative;
        }
    }
    excluded
}

fn select_by_numbers<R: BufRead, W: Write>(
    targets: Vec<CleanTarget>,
    input: &mut R,
    output: &mut W,
) -> Result<Vec<CleanTarget>> {
    let mut selected = vec![false; targets.len()];
    loop {
        write_numbered_targets(output, &targets, &selected)?;
        write!(output, "Select items to delete>> ")?;
        output.flush()?;
        let Some(line) = read_line(input)? else {
            return Ok(Vec::new());
        };
        if line.is_empty() {
            break;
        }
        apply_selection_line(&line, &targets, &mut selected, output)?;
    }
    Ok(targets
        .into_iter()
        .zip(selected)
        .filter_map(|(target, selected)| selected.then_some(target))
        .collect())
}

fn write_numbered_targets<W: Write>(
    output: &mut W,
    targets: &[CleanTarget],
    selected: &[bool],
) -> io::Result<()> {
    let items = targets
        .iter()
        .zip(selected)
        .enumerate()
        .map(|(index, (target, selected))| {
            format!(
                "{}{:2}: {}",
                if *selected { "*" } else { " " },
                index + 1,
                String::from_utf8_lossy(&target.display)
            )
        })
        .collect::<Vec<_>>();
    write_columns(output, &items, false)
}

/// Match Git's always-on interactive column table at `term_columns() - 1`.
/// Candidate lists use column-major order; numbered menus use row-major order.
fn write_columns<W: Write>(output: &mut W, items: &[String], column_major: bool) -> io::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    const INDENT: &str = "  ";
    const PADDING: usize = 2;
    let table_width = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(80)
        .saturating_sub(1);
    let maximum = items
        .iter()
        .map(|item| item.chars().count())
        .max()
        .unwrap_or(0);
    let cell_width = maximum + PADDING;
    let columns = (table_width.saturating_sub(INDENT.len()) / cell_width).max(1);
    let rows = items.len().div_ceil(columns);

    for row in 0..rows {
        write!(output, "{INDENT}")?;
        let mut column = 0usize;
        loop {
            let index = if column_major {
                column * rows + row
            } else {
                row * columns + column
            };
            if index >= items.len() {
                break;
            }
            let item = &items[index];
            write!(output, "{item}")?;
            if column + 1 >= columns {
                break;
            }
            let next = if column_major {
                (column + 1) * rows + row
            } else {
                row * columns + column + 1
            };
            if next >= items.len() {
                break;
            }
            let width = item.chars().count();
            write!(output, "{:padding$}", "", padding = cell_width - width)?;
            column += 1;
        }
        writeln!(output)?;
    }
    Ok(())
}

fn apply_selection_line<W: Write>(
    line: &str,
    targets: &[CleanTarget],
    selected: &mut [bool],
    output: &mut W,
) -> io::Result<()> {
    for raw in line.split(|character: char| character == ',' || character.is_whitespace()) {
        if raw.is_empty() {
            continue;
        }
        let (choose, token) = raw
            .strip_prefix('-')
            .map_or((true, raw), |token| (false, token));
        let Some((bottom, top)) = selection_bounds(token, targets) else {
            writeln!(output, "Huh ({token})?")?;
            continue;
        };
        for item in &mut selected[bottom..=top] {
            *item = choose;
        }
    }
    Ok(())
}

fn selection_bounds(token: &str, targets: &[CleanTarget]) -> Option<(usize, usize)> {
    if token == "*" {
        return (!targets.is_empty()).then_some((0, targets.len() - 1));
    }
    if token.bytes().all(|byte| byte.is_ascii_digit()) {
        let item = token.parse::<usize>().ok()?.checked_sub(1)?;
        return (item < targets.len()).then_some((item, item));
    }
    if let Some((left, right)) = token.split_once('-')
        && !left.is_empty()
        && !right.contains('-')
    {
        let bottom = left.parse::<usize>().ok()?.checked_sub(1)?;
        let top = if right.is_empty() {
            targets.len().checked_sub(1)?
        } else {
            right.parse::<usize>().ok()?.checked_sub(1)?
        };
        return (bottom <= top && top < targets.len()).then_some((bottom, top));
    }
    unique_target_prefix(token, targets).map(|index| (index, index))
}

fn unique_target_prefix(input: &str, targets: &[CleanTarget]) -> Option<usize> {
    let input = input.as_bytes();
    let mut found = None;
    for (index, target) in targets.iter().enumerate() {
        if target
            .display
            .get(..input.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(input))
        {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

fn ask_each<R: BufRead, W: Write>(
    targets: Vec<CleanTarget>,
    input: &mut R,
    output: &mut W,
) -> Result<Vec<CleanTarget>> {
    let mut selected = Vec::new();
    let mut eof = false;
    for target in targets {
        if eof {
            continue;
        }
        write!(
            output,
            "Remove {} [y/N]? ",
            String::from_utf8_lossy(&target.display)
        )?;
        output.flush()?;
        let Some(answer) = read_line(input)? else {
            writeln!(output)?;
            eof = true;
            continue;
        };
        if !answer.is_empty()
            && "yes"
                .get(..answer.len())
                .is_some_and(|yes| yes.eq_ignore_ascii_case(&answer))
        {
            selected.push(target);
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str) -> CleanTarget {
        CleanTarget {
            path: path.as_bytes().to_vec(),
            display: path.as_bytes().to_vec(),
            is_dir: path.ends_with('/'),
        }
    }

    #[test]
    fn menu_supports_hotkeys_and_unique_prefixes() {
        assert_eq!(unique_command("c"), Some('c'));
        assert_eq!(unique_command("cl"), Some('c'));
        assert_eq!(unique_command("quit"), Some('q'));
        assert_eq!(unique_command("2"), Some('f'));
    }

    #[test]
    fn pattern_negation_reincludes_later_matches() {
        assert!(!patterns_exclude(b"a.out", &["*", "!*.out"]));
        assert!(patterns_exclude(b"src/part3.c", &["*", "!*.out"]));
        assert!(patterns_exclude(b"../docs/", &["docs"]));
    }

    #[test]
    fn selection_supports_ranges_inverse_and_names() {
        let targets = vec![
            target("a.out"),
            target("bar.txt"),
            target("baz.txt"),
            target("foo.txt"),
            target("src/part4.c"),
        ];
        assert_eq!(selection_bounds("3-4", &targets), Some((2, 3)));
        assert_eq!(selection_bounds("4-", &targets), Some((3, 4)));
        assert_eq!(selection_bounds("fo", &targets), Some((3, 3)));
        assert_eq!(selection_bounds("ba", &targets), None);

        let mut selected = vec![true; targets.len()];
        apply_selection_line("-5- 1 -2", &targets, &mut selected, &mut Vec::new())
            .expect("apply inverse selection");
        assert_eq!(selected, vec![true, false, true, true, false]);
    }

    #[test]
    fn eof_and_ask_each_stop_without_selecting_remaining_targets() {
        let targets = vec![target("one"), target("two"), target("three")];
        let mut input = io::Cursor::new(b"yes\nno\n".to_vec());
        let selected = ask_each(targets, &mut input, &mut Vec::new()).expect("ask each selection");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].display, b"one");
    }
}
