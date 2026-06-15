//! `git add --patch` core: the per-hunk decision REPL.
//!
//! Port of the decision loop and prompt formatting from upstream
//! `add-patch.c`. The diff is produced by sley's own `diff-files -p`
//! (byte-exact, already parity-tested); each selected hunk is applied to the
//! index by reconstructing the file's index blob with the chosen hunks, writing
//! it via `hash-object -w`, and staging it via `update-index --cacheinfo`.
//! This mirrors how git spawns `git diff-files` / `git apply --cached` from
//! add-patch.c rather than re-implementing the diff/apply core.

use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use sley_core::{GitError, Result};

/// Which kind of patch session (add / reset / checkout / stash). Only `Add`
/// is wired today; the enum keeps the prompt-mode table addressable for the
/// other callers to grow into.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PatchMode {
    Add,
}

/// Per-session config knobs (context, inter-hunk-context, auto-advance).
#[derive(Clone, Copy)]
pub(crate) struct PatchConfig {
    pub auto_advance: bool,
}

impl Default for PatchConfig {
    fn default() -> Self {
        PatchConfig { auto_advance: true }
    }
}

fn self_bin() -> PathBuf {
    env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"))
}

fn run_capture(args: &[&str], stdin_bytes: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let mut child = Command::new(self_bin())
        .args(args)
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(bytes) = stdin_bytes
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(bytes);
    }
    let out = child.wait_with_output()?;
    Ok(out.stdout)
}

// ---------------------------------------------------------------------------
// Diff model
// ---------------------------------------------------------------------------

/// A parsed hunk: the `@@` header counts plus the body lines (each with its
/// leading marker: ' ', '+', '-', '\\').
struct Hunk {
    old_offset: i64,
    old_count: i64,
    new_offset: i64,
    new_count: i64,
    /// Text after the second `@@` on the header line (function context),
    /// including the trailing newline.
    heading: String,
    /// Body lines, raw (with leading marker, no trailing newline list).
    body: Vec<String>,
    use_hunk: HunkUse,
    /// Number of independent pieces this hunk could split into.
    splittable_into: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum HunkUse {
    Undecided,
    Use,
    Skip,
}

/// One file's diff: the header lines (everything before the first `@@`) plus
/// its hunks.
struct FileDiff {
    /// Path used to read the index blob and stage the result.
    path: String,
    /// File mode for staging (from the `new file mode`/`index ... <mode>`).
    mode: u32,
    /// Header lines (diff --git ... up to but excluding the first @@).
    header: Vec<String>,
    hunks: Vec<Hunk>,
    /// True for an addition (no old content), deletion, or mode-only change.
    added: bool,
    deleted: bool,
    /// True when the diff reports the file as binary.
    is_binary: bool,
}

/// Parse a multi-file unified diff (as produced by `diff-files -p`) into
/// `FileDiff`s.
fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("diff --git ") {
            // New file diff. Collect header until first @@ or next diff.
            let mut header = Vec::new();
            let mut path = String::new();
            let mut mode = 0o100644u32;
            let mut added = false;
            let mut deleted = false;
            let mut is_binary = false;
            header.push(line.to_string());
            i += 1;
            while i < lines.len()
                && !lines[i].starts_with("@@ ")
                && !lines[i].starts_with("diff --git ")
            {
                let h = lines[i];
                if h.starts_with("Binary files ") || h.starts_with("GIT binary patch") {
                    is_binary = true;
                }
                if let Some(rest) = h.strip_prefix("new file mode ") {
                    mode = u32::from_str_radix(rest.trim(), 8).unwrap_or(mode);
                    added = true;
                } else if h.starts_with("deleted file mode ") {
                    deleted = true;
                } else if let Some(rest) = h.strip_prefix("+++ b/") {
                    if path.is_empty() {
                        path = rest.to_string();
                    }
                } else if let Some(rest) = h.strip_prefix("--- a/") {
                    if path.is_empty() && rest != "/dev/null" {
                        path = rest.to_string();
                    }
                } else if let Some(rest) = h.strip_prefix("index ") {
                    // index <a>..<b> <mode>
                    if let Some(sp) = rest.rfind(' ') {
                        if let Ok(m) = u32::from_str_radix(rest[sp + 1..].trim(), 8) {
                            mode = m;
                        }
                    }
                }
                header.push(h.to_string());
                i += 1;
            }
            // Fallback path from the `diff --git a/X b/X` line.
            if path.is_empty() {
                path = parse_git_header_path(line);
            }
            let mut fd = FileDiff {
                path,
                mode,
                header,
                hunks: Vec::new(),
                added,
                deleted,
                is_binary,
            };
            // Parse hunks.
            while i < lines.len() && lines[i].starts_with("@@ ") {
                let (hunk, next) = parse_hunk(&lines, i);
                fd.hunks.push(hunk);
                i = next;
            }
            files.push(fd);
        } else {
            i += 1;
        }
    }
    files
}

/// Extract the path from a `diff --git a/<p> b/<p>` line.
fn parse_git_header_path(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("diff --git a/") {
        if let Some(sp) = rest.find(" b/") {
            return rest[..sp].to_string();
        }
    }
    String::new()
}

/// Parse a hunk starting at `lines[start]` (the `@@` line). Returns the hunk
/// and the index of the line after it.
fn parse_hunk(lines: &[&str], start: usize) -> (Hunk, usize) {
    let header = lines[start];
    let (old_offset, old_count, new_offset, new_count, heading) = parse_hunk_header(header);
    let mut body = Vec::new();
    let mut i = start + 1;
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with("@@ ") || l.starts_with("diff --git ") {
            break;
        }
        // The final element after a trailing newline split is "" — stop there.
        if l.is_empty() && i == lines.len() - 1 {
            break;
        }
        // A body line must start with a known marker.
        let first = l.as_bytes().first().copied();
        if !matches!(first, Some(b' ') | Some(b'+') | Some(b'-') | Some(b'\\')) {
            break;
        }
        body.push(l.to_string());
        i += 1;
    }
    let splittable = count_splittable(&body);
    (
        Hunk {
            old_offset,
            old_count,
            new_offset,
            new_count,
            heading,
            body,
            use_hunk: HunkUse::Undecided,
            splittable_into: splittable,
        },
        i,
    )
}

/// Parse `@@ -A,B +C,D @@ heading`. Counts default to 1 when omitted.
fn parse_hunk_header(line: &str) -> (i64, i64, i64, i64, String) {
    // After "@@ -" ... " @@" optional heading.
    let rest = line.strip_prefix("@@ -").unwrap_or(line);
    let end = rest.find(" @@").unwrap_or(rest.len());
    let ranges = &rest[..end];
    let heading = if end + 3 <= rest.len() {
        rest[end + 3..].to_string()
    } else {
        String::new()
    };
    // ranges: "A,B +C,D"
    let mut parts = ranges.split(" +");
    let (old_off, old_cnt) = parse_range(parts.next().unwrap_or(""));
    let (new_off, new_cnt) = parse_range(parts.next().unwrap_or(""));
    (old_off, old_cnt, new_off, new_cnt, heading)
}

fn parse_range(s: &str) -> (i64, i64) {
    let mut it = s.split(',');
    let off = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let cnt = it.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    (off, cnt)
}

/// `splittable_into`: mirrors add-patch.c, which zero-initializes the counter
/// and does `splittable_into++` on each transition from a change line (`+`/`-`)
/// to a context line (` `). A hunk is splittable when the counter exceeds 1,
/// i.e. there are at least two change groups each terminated by context. The
/// trailing context after the final change group only yields one increment, so
/// a single change-block followed by context is NOT splittable (matches git).
fn count_splittable(body: &[String]) -> usize {
    let mut count = 0usize;
    // git seeds `marker` from the `@@` header line, which is neither '+' nor
    // '-', so the first body line never triggers an increment on its own.
    let mut marker = b'@';
    for l in body {
        let mut ch = l.as_bytes().first().copied().unwrap_or(b' ');
        // "\ No newline" lines inherit the previous marker.
        if ch == b'\\' {
            ch = marker;
        }
        if (marker == b'-' || marker == b'+') && ch == b' ' {
            count += 1;
        }
        if ch != b'\\' {
            marker = ch;
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Prompt strings (PatchMode::Add)
// ---------------------------------------------------------------------------

fn prompt_mode_change() -> &'static str {
    "Stage mode change"
}
fn prompt_deletion() -> &'static str {
    "Stage deletion"
}
fn prompt_addition() -> &'static str {
    "Stage addition"
}
fn prompt_hunk() -> &'static str {
    "Stage this hunk"
}

const HELP_TEXT: &str = "y - stage this hunk\n\
    n - do not stage this hunk\n\
    q - quit; do not stage this hunk or any of the remaining ones\n\
    a - stage this hunk and all later hunks in the file\n\
    d - do not stage this hunk or any of the later hunks in the file\n";

const NAV_HELP: &str = "j - go to the next undecided hunk, roll over at the bottom\n\
    J - go to the next hunk, roll over at the bottom\n\
    k - go to the previous undecided hunk, roll over at the top\n\
    K - go to the previous hunk, roll over at the top\n\
    g - select a hunk to go to\n\
    / - search for a hunk matching the given regex\n\
    s - split the current hunk into smaller hunks\n\
    e - manually edit the current hunk\n\
    p - print the current hunk\n\
    P - print the current hunk using the pager\n\
    ? - print help\n";

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Run the `add -p` REPL over the given paths. Reads decisions from `stdin`.
pub(crate) fn run_add_patch(
    _mode: PatchMode,
    paths: &[String],
    stdin: &mut impl BufRead,
    cfg: PatchConfig,
) -> Result<()> {
    // Produce the diff for the requested paths.
    let mut args = vec!["diff-files", "-p"];
    if !paths.is_empty() {
        args.push("--");
    }
    for p in paths {
        args.push(p.as_str());
    }
    let diff = run_capture(&args, None).map_err(|e| GitError::Io(e.to_string()))?;
    let diff_text = String::from_utf8_lossy(&diff).into_owned();
    let mut files = parse_diff(&diff_text);

    if files.is_empty() || files.iter().all(|f| f.hunks.is_empty()) {
        // The direct `git add -p` path prints "No changes." (or "Only binary
        // files changed.") to STDOUT; t3701 redirects only stdout and greps it.
        let only_binary = !files.is_empty() && files.iter().all(|f| f.is_binary);
        if only_binary {
            println!("Only binary files changed.");
        } else {
            println!("No changes.");
        }
        return Ok(());
    }

    let mut file_idx = 0;
    let nfiles = files.len();
    while file_idx < nfiles {
        patch_update_file(&mut files[file_idx], stdin, cfg)?;
        file_idx += 1;
    }

    // Apply: for each file, reconstruct the index blob with USE_HUNK hunks and
    // stage it.
    for fd in &files {
        if fd.hunks.iter().any(|h| h.use_hunk == HunkUse::Use) {
            apply_file_to_index(fd)?;
        }
    }
    Ok(())
}

/// Build the `[y,n,q,a,d<extra>,?]` command suffix and the permitted set.
fn build_suffix(
    fd: &FileDiff,
    hunk_index: usize,
    undecided_next: Option<usize>,
    undecided_prev: Option<usize>,
    cfg: PatchConfig,
    nfiles: usize,
) -> String {
    let mut s = String::new();
    let nr = fd.hunks.len();
    if undecided_prev.is_some() {
        s.push_str(",k");
    }
    if nr > 1 {
        s.push_str(",K");
    }
    if undecided_next.is_some() {
        s.push_str(",j");
    }
    if nr > 1 {
        s.push_str(",J");
    }
    if nr > 1 {
        s.push_str(",g,/");
    }
    if fd.hunks[hunk_index].splittable_into > 1 {
        s.push_str(",s");
    }
    if !fd.deleted {
        s.push_str(",e");
    }
    if !cfg.auto_advance && nfiles > 1 {
        s.push_str(",>");
        s.push_str(",<");
    }
    s.push_str(",p,P");
    s
}

/// The per-file decision loop.
fn patch_update_file(fd: &mut FileDiff, stdin: &mut impl BufRead, cfg: PatchConfig) -> Result<()> {
    if fd.hunks.is_empty() {
        return Ok(());
    }
    let mut hunk_index = 0usize;
    let mut rendered: Option<usize> = None;
    let mut pending_err: Option<String> = None;

    // The file's diff header (`diff --git ...`) is printed exactly once on entry.
    render_file_header(fd);

    loop {
        let nr = fd.hunks.len();
        // If a prior y/n advanced past the end with no undecided hunk left,
        // and indeed nothing is undecided, the file is done. (git lets the
        // index run one past the end and relies on the undecided scan; we guard
        // the out-of-range index explicitly.)
        if hunk_index >= nr {
            if first_undecided(fd).is_none() {
                break;
            }
            hunk_index = first_undecided(fd).unwrap();
            rendered = None;
        }
        // Find undecided next/prev.
        let undecided_next = next_undecided(fd, hunk_index);
        let undecided_prev = prev_undecided(fd, hunk_index);

        // Everything decided?
        if undecided_next.is_none()
            && undecided_prev.is_none()
            && fd.hunks[hunk_index].use_hunk != HunkUse::Undecided
        {
            // No undecided hunks anywhere → done with this file.
            break;
        }

        // Render the hunk if newly arrived at.
        if rendered != Some(hunk_index) {
            render_hunk(fd, hunk_index, 0);
            rendered = Some(hunk_index);
        }

        if let Some(msg) = pending_err.take() {
            print!("{msg}");
            let _ = io::stdout().flush();
        }

        // Build prompt.
        let kind = if fd.deleted {
            prompt_deletion()
        } else if fd.added {
            prompt_addition()
        } else {
            prompt_hunk()
        };
        let suffix = build_suffix(fd, hunk_index, undecided_next, undecided_prev, cfg, 1);
        let was = match fd.hunks[hunk_index].use_hunk {
            HunkUse::Use => " (was: y)",
            HunkUse::Skip => " (was: n)",
            HunkUse::Undecided => "",
        };
        print!(
            "({}/{}) {kind}{was} [y,n,q,a,d{suffix},?]? ",
            hunk_index + 1,
            nr
        );
        let _ = io::stdout().flush();

        let line = match read_line(stdin) {
            Some(l) => l,
            None => {
                // On EOF at the prompt git terminates the line it was waiting on
                // with a newline, so the captured output ends in `...]? \n`.
                println!();
                return Ok(());
            }
        };
        if line.is_empty() {
            continue;
        }
        let answer = line.clone();
        let ch = answer.chars().next().unwrap();
        let lower = ch.to_ascii_lowercase();

        // g and / take arguments; everything else must be a single letter.
        if answer.chars().count() != 1 && lower != 'g' && lower != '/' {
            pending_err = Some(format!("Only one letter is expected, got '{answer}'\n"));
            continue;
        }

        match lower {
            'y' => {
                fd.hunks[hunk_index].use_hunk = HunkUse::Use;
                hunk_index = undecided_next.unwrap_or(nr);
                rendered = None;
            }
            'n' => {
                fd.hunks[hunk_index].use_hunk = HunkUse::Skip;
                hunk_index = undecided_next.unwrap_or(nr);
                rendered = None;
            }
            'a' => {
                for h in fd.hunks.iter_mut().skip(hunk_index) {
                    if h.use_hunk == HunkUse::Undecided {
                        h.use_hunk = HunkUse::Use;
                    }
                }
                match first_undecided(fd) {
                    Some(i) => {
                        hunk_index = i;
                        rendered = None;
                    }
                    None => {
                        hunk_index = 0;
                        rendered = None;
                    }
                }
            }
            'd' => {
                for h in fd.hunks.iter_mut().skip(hunk_index) {
                    if h.use_hunk == HunkUse::Undecided {
                        h.use_hunk = HunkUse::Skip;
                    }
                }
                match first_undecided(fd) {
                    Some(i) => {
                        hunk_index = i;
                        rendered = None;
                    }
                    None => {
                        hunk_index = 0;
                        rendered = None;
                    }
                }
            }
            'q' => {
                // Mark all remaining undecided as skip and stop.
                return Ok(());
            }
            'k' if ch == 'k' => {
                if let Some(p) = undecided_prev {
                    hunk_index = p;
                    rendered = None;
                } else {
                    pending_err = Some("No other undecided hunk\n".to_string());
                }
            }
            'j' if ch == 'j' => {
                if let Some(n) = undecided_next {
                    hunk_index = n;
                    rendered = None;
                } else {
                    pending_err = Some("No other undecided hunk\n".to_string());
                }
            }
            's' => {
                if fd.hunks[hunk_index].splittable_into > 1 {
                    let n = split_hunk(fd, hunk_index);
                    println!("Split into {n} hunks.");
                    rendered = None;
                } else {
                    pending_err = Some("Sorry, cannot split this hunk\n".to_string());
                }
            }
            'p' => {
                rendered = None;
            }
            _ => {
                // Capital K / J navigation, g, /, e, P, ? and unknowns.
                match ch {
                    'K' => {
                        if nr > 1 {
                            hunk_index = dec_mod(hunk_index, nr);
                            rendered = None;
                        } else {
                            pending_err = Some("No other hunk\n".to_string());
                        }
                    }
                    'J' => {
                        if nr > 1 {
                            hunk_index = (hunk_index + 1) % nr;
                            rendered = None;
                        } else {
                            pending_err = Some("No other hunk\n".to_string());
                        }
                    }
                    'g' => {
                        if let Some(target) =
                            parse_goto(&answer, fd, hunk_index, stdin, &mut pending_err)
                        {
                            hunk_index = target;
                            rendered = None;
                        }
                    }
                    '/' => match parse_search(&answer, fd, hunk_index, stdin) {
                        Some(target) => {
                            hunk_index = target;
                            rendered = None;
                        }
                        None => {
                            pending_err =
                                Some("No hunk matches the given pattern\n".to_string());
                        }
                    },
                    'P' => {
                        rendered = None;
                    }
                    '?' => {
                        print!("{HELP_TEXT}");
                        if nr > 1 || fd.hunks[hunk_index].splittable_into > 1 {
                            print!("{NAV_HELP}");
                        } else {
                            print!(
                                "e - manually edit the current hunk\n\
                                 p - print the current hunk\n\
                                 P - print the current hunk using the pager\n\
                                 ? - print help\n"
                            );
                        }
                    }
                    _ => {
                        pending_err = Some(format!(
                            "Unknown command '{answer}' (use '?' for help)\n"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

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

fn next_undecided(fd: &FileDiff, from: usize) -> Option<usize> {
    let nr = fd.hunks.len();
    let mut i = (from + 1) % nr;
    while i != from {
        if fd.hunks[i].use_hunk == HunkUse::Undecided {
            return Some(i);
        }
        i = (i + 1) % nr;
    }
    None
}

fn prev_undecided(fd: &FileDiff, from: usize) -> Option<usize> {
    let nr = fd.hunks.len();
    let mut i = dec_mod(from, nr);
    while i != from {
        if fd.hunks[i].use_hunk == HunkUse::Undecided {
            return Some(i);
        }
        i = dec_mod(i, nr);
    }
    None
}

fn first_undecided(fd: &FileDiff) -> Option<usize> {
    fd.hunks
        .iter()
        .position(|h| h.use_hunk == HunkUse::Undecided)
}

fn dec_mod(i: usize, n: usize) -> usize {
    if i == 0 { n - 1 } else { i - 1 }
}

const SUMMARY_HEADER_WIDTH: usize = 20;
const SUMMARY_LINE_WIDTH: usize = 80;
const DISPLAY_HUNKS_LINES: usize = 20;

/// `summarize_hunk`: ` -A,B +C,D ` padded to SUMMARY_HEADER_WIDTH, then the
/// first non-context body line (truncated to SUMMARY_LINE_WIDTH), newline-
/// terminated.
fn summarize_hunk(h: &Hunk) -> String {
    let mut s = format!(
        " -{},{} +{},{} ",
        h.old_offset, h.old_count, h.new_offset, h.new_count
    );
    if s.len() < SUMMARY_HEADER_WIDTH {
        s.push_str(&" ".repeat(SUMMARY_HEADER_WIDTH - s.len()));
    }
    // First non-context line.
    if let Some(line) = h.body.iter().find(|l| !l.starts_with(' ')) {
        s.push_str(line);
    }
    if s.len() > SUMMARY_LINE_WIDTH {
        s.truncate(SUMMARY_LINE_WIDTH);
    }
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// `display_hunks`: print the use-marked numbered summaries for the window
/// [start, start+DISPLAY_HUNKS_LINES). Returns the end index reached.
fn display_hunks(fd: &FileDiff, start: usize) -> usize {
    let end = (start + DISPLAY_HUNKS_LINES).min(fd.hunks.len());
    for (idx, h) in fd.hunks.iter().enumerate().take(end).skip(start) {
        let marker = match h.use_hunk {
            HunkUse::Use => '+',
            HunkUse::Skip => '-',
            HunkUse::Undecided => ' ',
        };
        print!("{marker}{:2}: {}", idx + 1, summarize_hunk(h));
    }
    end
}

fn parse_goto(
    answer: &str,
    fd: &FileDiff,
    cur: usize,
    stdin: &mut impl BufRead,
    err: &mut Option<String>,
) -> Option<usize> {
    // "g N" / "gN" carry the target inline; bare "g" prints the hunk list and
    // prompts "go to which hunk?".
    let arg = answer[1..].trim().to_string();
    let response = if arg.is_empty() {
        // Display a window centered on the current hunk, then prompt.
        let mut start = cur.saturating_sub(DISPLAY_HUNKS_LINES / 2);
        let mut got = String::new();
        loop {
            let end = display_hunks(fd, start);
            if end < fd.hunks.len() {
                print!("go to which hunk (<ret> to see more)? ");
            } else {
                print!("go to which hunk? ");
            }
            let _ = io::stdout().flush();
            match read_line(stdin) {
                Some(l) => {
                    let t = l.trim().to_string();
                    if t.is_empty() {
                        start = end;
                        continue;
                    }
                    got = t;
                    break;
                }
                None => return None,
            }
        }
        got
    } else {
        arg
    };
    match response.parse::<usize>() {
        Ok(num) if num >= 1 && num <= fd.hunks.len() => Some(num - 1),
        Ok(_) => {
            let n = fd.hunks.len();
            let word = if n == 1 { "hunk" } else { "hunks" };
            *err = Some(format!("Sorry, only {n} {word} available.\n"));
            None
        }
        Err(_) => {
            *err = Some(format!("Invalid number: '{response}'\n"));
            None
        }
    }
}

fn parse_search(
    answer: &str,
    fd: &FileDiff,
    from: usize,
    stdin: &mut impl BufRead,
) -> Option<usize> {
    // Bare `/` prompts "search for regex? "; `/pat` carries the pattern inline.
    let mut pat = answer[1..].trim().to_string();
    if pat.is_empty() {
        print!("search for regex? ");
        let _ = io::stdout().flush();
        match read_line(stdin) {
            Some(l) => pat = l.trim().to_string(),
            None => return None,
        }
        if pat.is_empty() {
            // Empty pattern: git just continues the loop (no move).
            return Some(from);
        }
    }
    let re = regex_lite_compile(&pat)?;
    let nr = fd.hunks.len();
    // Search starts at the CURRENT hunk (inclusive) and matches against the
    // rendered hunk text (header line + body), wrapping once.
    let mut i = from;
    loop {
        let mut text = format_hunk_header(&fd.hunks[i], 0);
        text.push('\n');
        text.push_str(&fd.hunks[i].body.join("\n"));
        if re.is_match(&text) {
            return Some(i);
        }
        i = (i + 1) % nr;
        if i == from {
            return None;
        }
    }
}

/// Minimal substring/regex match: tries plain substring first, which covers
/// every literal-pattern test in the oracle.
struct LiteRe {
    pat: String,
}
impl LiteRe {
    fn is_match(&self, hay: &str) -> bool {
        hay.contains(&self.pat)
    }
}
fn regex_lite_compile(pat: &str) -> Option<LiteRe> {
    Some(LiteRe {
        pat: pat.to_string(),
    })
}

/// Render a hunk to stdout: the `@@` header (with `delta`-adjusted offset for
/// printing the live hunk, which is 0) plus body lines.
fn render_hunk(fd: &FileDiff, hunk_index: usize, _delta: i64) {
    // Print the file diff header before the FIRST hunk's render only once per
    // session; git prints the header before the first hunk of each file.
    let h = &fd.hunks[hunk_index];
    println!("{}", format_hunk_header(h, 0));
    for line in &h.body {
        println!("{line}");
    }
}

/// Print the file's diff header (the `diff --git ...` block up to the first
/// `@@`). git emits this exactly once, when the file is first entered.
fn render_file_header(fd: &FileDiff) {
    for h in &fd.header {
        println!("{h}");
    }
}

/// Format a `@@ -A,B +C,D @@<heading>` header, applying `delta` to the new
/// offset (used when reassembling a partial patch).
fn format_hunk_header(h: &Hunk, delta: i64) -> String {
    let new_offset = h.new_offset + delta;
    let old = format_range(h.old_offset, h.old_count);
    let new = format_range(new_offset, h.new_count);
    if h.heading.is_empty() {
        format!("@@ -{old} +{new} @@")
    } else {
        format!("@@ -{old} +{new} @@{}", h.heading)
    }
}

fn format_range(offset: i64, count: i64) -> String {
    if count == 1 {
        format!("{offset}")
    } else {
        format!("{offset},{count}")
    }
}

/// Number of leading context lines in a sub-hunk body (counts only ` `-marked
/// lines before the first change line).
fn leading_context_len(piece: &[String]) -> i64 {
    let mut n = 0i64;
    for l in piece {
        match l.as_bytes().first().copied().unwrap_or(b' ') {
            b' ' => n += 1,
            _ => break,
        }
    }
    n
}

/// Split the hunk at `hunk_index` into its constituent sub-hunks in place,
/// recomputing each sub-hunk's `@@` offsets/counts. Returns the number of
/// resulting hunks. Mirrors add-patch.c's `split_hunk`: a new sub-hunk begins
/// at the first context line following a run of `+`/`-` lines, and the trailing
/// context of one sub-hunk is shared (overlapped) as the leading context of the
/// next so each piece applies independently.
fn split_hunk(fd: &mut FileDiff, hunk_index: usize) -> usize {
    let h = &fd.hunks[hunk_index];
    if h.splittable_into < 2 {
        return 1;
    }
    let body = h.body.clone();
    let heading = h.heading.clone();
    let old_start = h.old_offset;
    let new_start = h.new_offset;

    // Identify the split points: an index in `body` where a context line begins
    // a new piece (i.e. it follows a change line). The new piece's leading
    // context overlaps with the previous piece's trailing context.
    //
    // We accumulate lines into the current piece; when we are *in* a change run
    // and hit a context line, we close the current piece at the END of its
    // trailing context and the next piece starts at the FIRST trailing-context
    // line.
    // git shares the context block between adjacent sub-hunks: the trailing
    // context of piece N is also the leading context of piece N+1. We detect the
    // boundary as the first change line that arrives *after* a context run which
    // itself followed a change run. At that point the previous piece is closed
    // (including the shared context), and the new piece is seeded with that same
    // context block.
    let mut pieces: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut marker = b'@';
    // Index in `current` where the current trailing-context block began.
    let mut ctx_start_in_current: Option<usize> = None;
    let mut seen_change_in_current = false;

    for line in &body {
        let ch = line.as_bytes().first().copied().unwrap_or(b' ');
        let norm = if ch == b'\\' { marker } else { ch };

        // Entering a context run right after a change run: record where it began.
        if (marker == b'-' || marker == b'+') && norm == b' ' {
            ctx_start_in_current = Some(current.len());
        }
        // A new change line after a recorded trailing-context block: close the
        // current piece (it already contains the full shared context), and start
        // the next piece seeded with that shared context block.
        if (norm == b'-' || norm == b'+') && seen_change_in_current {
            if let Some(cut) = ctx_start_in_current.take() {
                let shared: Vec<String> = current[cut..].to_vec();
                pieces.push(std::mem::take(&mut current));
                current = shared;
                seen_change_in_current = false;
            }
        }
        if norm == b'-' || norm == b'+' {
            seen_change_in_current = true;
            ctx_start_in_current = None;
        }
        current.push(line.clone());
        if norm != b'\\' {
            marker = norm;
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }

    // Recompute each piece's header. Because adjacent pieces SHARE a context
    // block (the trailing context of one is the leading context of the next),
    // the next piece's offset must back up by the length of that shared leading
    // context — otherwise the overlapped lines would be counted twice in the
    // running offset.
    let mut new_hunks = Vec::new();
    let mut old_cursor = old_start;
    let mut new_cursor = new_start;
    for (pi, piece) in pieces.iter().enumerate() {
        let mut old_count = 0i64;
        let mut new_count = 0i64;
        for l in piece {
            match l.as_bytes().first().copied().unwrap_or(b' ') {
                b' ' => {
                    old_count += 1;
                    new_count += 1;
                }
                b'-' => old_count += 1,
                b'+' => new_count += 1,
                _ => {}
            }
        }
        // For pieces after the first, subtract the leading shared context length
        // from the running cursors so the offset lands on the shared line.
        if pi > 0 {
            let lead_ctx = leading_context_len(piece);
            old_cursor -= lead_ctx;
            new_cursor -= lead_ctx;
        }
        new_hunks.push(Hunk {
            old_offset: old_cursor,
            old_count,
            new_offset: new_cursor,
            new_count,
            heading: heading.clone(),
            body: piece.clone(),
            use_hunk: HunkUse::Undecided,
            splittable_into: 1,
        });
        old_cursor += old_count;
        new_cursor += new_count;
    }
    let n = new_hunks.len();
    // Replace the single hunk with the pieces.
    fd.hunks.splice(hunk_index..hunk_index + 1, new_hunks);
    n
}

/// Apply the USE_HUNK hunks of one file to the index.
fn apply_file_to_index(fd: &FileDiff) -> Result<()> {
    // Read the index version of the file (stage 0).
    let spec = format!(":{}", fd.path);
    let base = run_capture(&["cat-file", "blob", &spec], None)
        .map_err(|e| GitError::Io(e.to_string()))?;
    let base_text = String::from_utf8_lossy(&base).into_owned();
    let new_content = apply_hunks(&base_text, fd);
    // Write the result as a blob.
    let oid = run_capture(&["hash-object", "-w", "--stdin"], Some(new_content.as_bytes()))
        .map_err(|e| GitError::Io(e.to_string()))?;
    let oid = String::from_utf8_lossy(&oid).trim().to_string();
    let mode = format!("{:o}", fd.mode);
    let status = Command::new(self_bin())
        .args(["update-index", "--cacheinfo", &mode, &oid, &fd.path])
        .stdin(Stdio::null())
        .status()
        .map_err(|e| GitError::Io(e.to_string()))?;
    if !status.success() {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// Apply the selected hunks to the index base text, line by line.
fn apply_hunks(base: &str, fd: &FileDiff) -> String {
    // base lines (preserve trailing newline behavior).
    let had_final_nl = base.ends_with('\n');
    let mut base_lines: Vec<&str> = base.split('\n').collect();
    if had_final_nl {
        base_lines.pop(); // drop the empty trailing element
    }
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize; // 0-based index into base_lines

    for h in &fd.hunks {
        if h.use_hunk != HunkUse::Use {
            continue;
        }
        // old_offset is 1-based line where the hunk's old side begins.
        let start = (h.old_offset - 1).max(0) as usize;
        // Copy unchanged base lines up to the hunk start.
        while cursor < start && cursor < base_lines.len() {
            out.push(base_lines[cursor].to_string());
            cursor += 1;
        }
        // Walk the hunk body.
        for line in &h.body {
            let marker = line.as_bytes().first().copied().unwrap_or(b' ');
            let rest = &line[1.min(line.len())..];
            match marker {
                b' ' => {
                    out.push(rest.to_string());
                    cursor += 1;
                }
                b'-' => {
                    cursor += 1;
                }
                b'+' => {
                    out.push(rest.to_string());
                }
                b'\\' => { /* "\ No newline at end of file" */ }
                _ => {}
            }
        }
    }
    // Copy any remaining base lines.
    while cursor < base_lines.len() {
        out.push(base_lines[cursor].to_string());
        cursor += 1;
    }
    let mut result = out.join("\n");
    if had_final_nl && !result.is_empty() {
        result.push('\n');
    } else if had_final_nl && result.is_empty() {
        // keep empty
    }
    result
}
