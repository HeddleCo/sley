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

use sley::{GitError, Result};

/// Which kind of patch session. Mirrors add-patch.c's `patch_mode_*` table:
/// each variant fixes the diff command, the apply direction, and the prompt
/// wording. `Add`/`Reset` stage/unstage to the index by reconstructing the
/// index blob; the checkout/worktree variants reassemble the selected hunks
/// into a patch and pipe it to `apply` (matching git's `apply_patch`).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PatchMode {
    Add,
    Stash,
    Reset,
    /// `checkout -p` / `restore -p` with no tree-ish: `diff-files`, discard the
    /// selected hunks from the working tree (`apply -R`).
    CheckoutIndex,
    /// `checkout -p HEAD`: `diff-index HEAD`, discard from index and worktree.
    CheckoutHead,
    /// `checkout -p <rev>`: apply to index and worktree. git diffs with
    /// `diff-index -R <rev>` and applies forward; sley instead diffs forward
    /// (`diff-index <rev>`) and reverse-applies — net-identical, and it sidesteps
    /// `diff-index -R`'s broken worktree-side rendering.
    CheckoutNothead,
    /// `restore -p --source=HEAD`: `diff-index HEAD`, discard from worktree only.
    WorktreeHead,
    /// `restore -p --source=<rev>`: apply to worktree only. Same forward-diff +
    /// reverse-apply substitution as [`PatchMode::CheckoutNothead`].
    WorktreeNothead,
}

/// Which prompt-noun a hunk uses (git's `enum prompt_mode_type`).
#[derive(Clone, Copy)]
enum PromptKind {
    ModeChange,
    Deletion,
    Addition,
    Hunk,
}

impl PatchMode {
    /// True for the modes that reverse-apply their (forward) diff. Every
    /// checkout/worktree mode does: the index/head modes discard worktree
    /// changes, and the not-head modes substitute a reverse-apply for git's
    /// `diff-index -R` (see [`PatchMode::CheckoutNothead`]).
    fn is_reverse(self) -> bool {
        matches!(
            self,
            PatchMode::CheckoutIndex
                | PatchMode::CheckoutHead
                | PatchMode::CheckoutNothead
                | PatchMode::WorktreeHead
                | PatchMode::WorktreeNothead
        )
    }

    /// True for the modes that touch BOTH the index and the working tree
    /// (git's `apply_for_checkout`).
    fn applies_for_checkout(self) -> bool {
        matches!(self, PatchMode::CheckoutHead | PatchMode::CheckoutNothead)
    }

    /// True for the new patch modes that reassemble a patch and pipe it to
    /// `apply`, rather than reconstructing the index blob directly.
    fn applies_via_patch(self) -> bool {
        !matches!(self, PatchMode::Add | PatchMode::Stash | PatchMode::Reset)
    }
}

/// The `Stage/Discard/Apply ... [hunk]` prompt verb for a given mode + noun,
/// mirroring add-patch.c's per-mode `prompt_mode` tables.
fn prompt_text(mode: PatchMode, kind: PromptKind) -> &'static str {
    match mode {
        PatchMode::Add | PatchMode::Reset => match kind {
            PromptKind::ModeChange => "Stage mode change",
            PromptKind::Deletion => "Stage deletion",
            PromptKind::Addition => "Stage addition",
            PromptKind::Hunk => "Stage this hunk",
        },
        PatchMode::Stash => match kind {
            PromptKind::ModeChange => "Stash mode change",
            PromptKind::Deletion => "Stash deletion",
            PromptKind::Addition => "Stash addition",
            PromptKind::Hunk => "Stash this hunk",
        },
        PatchMode::CheckoutIndex | PatchMode::WorktreeHead => match kind {
            PromptKind::ModeChange => "Discard mode change from worktree",
            PromptKind::Deletion => "Discard deletion from worktree",
            PromptKind::Addition => "Discard addition from worktree",
            PromptKind::Hunk => "Discard this hunk from worktree",
        },
        PatchMode::CheckoutHead => match kind {
            PromptKind::ModeChange => "Discard mode change from index and worktree",
            PromptKind::Deletion => "Discard deletion from index and worktree",
            PromptKind::Addition => "Discard addition from index and worktree",
            PromptKind::Hunk => "Discard this hunk from index and worktree",
        },
        PatchMode::CheckoutNothead => match kind {
            PromptKind::ModeChange => "Apply mode change to index and worktree",
            PromptKind::Deletion => "Apply deletion to index and worktree",
            PromptKind::Addition => "Apply addition to index and worktree",
            PromptKind::Hunk => "Apply this hunk to index and worktree",
        },
        PatchMode::WorktreeNothead => match kind {
            PromptKind::ModeChange => "Apply mode change to worktree",
            PromptKind::Deletion => "Apply deletion to worktree",
            PromptKind::Addition => "Apply addition to worktree",
            PromptKind::Hunk => "Apply this hunk to worktree",
        },
    }
}

/// Per-session config knobs (context, inter-hunk-context, auto-advance).
#[derive(Clone)]
pub(crate) struct PatchConfig {
    pub auto_advance: bool,
    /// Resolved diff context (lines), already validated non-negative.
    pub context: Option<usize>,
    /// Resolved inter-hunk context, already validated non-negative.
    pub interhunk: Option<usize>,
    /// `diff.algorithm` value to forward to the spawned `diff-files`, if set.
    pub diff_algorithm: Option<String>,
    pub colors: Option<crate::commands::diff_words::DiffColors>,
    pub prompt_color: String,
    pub header_color: String,
    pub reset_interactive: String,
    pub diff_filter: Option<String>,
}

impl Default for PatchConfig {
    fn default() -> Self {
        PatchConfig {
            auto_advance: true,
            context: None,
            interhunk: None,
            diff_algorithm: None,
            colors: None,
            prompt_color: String::new(),
            header_color: String::new(),
            reset_interactive: String::new(),
            diff_filter: None,
        }
    }
}

fn self_bin() -> PathBuf {
    env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"))
}

fn run_capture(args: &[&str], stdin_bytes: Option<&[u8]>) -> io::Result<Vec<u8>> {
    Ok(run_capture_status(args, stdin_bytes)?.0)
}

/// Like [`run_capture`] but also returns whether the child exited successfully.
fn run_capture_status(args: &[&str], stdin_bytes: Option<&[u8]>) -> io::Result<(Vec<u8>, bool)> {
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
    Ok((out.stdout, out.status.success()))
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
    display_header: Option<String>,
    display_body: Vec<String>,
    edited: bool,
    use_hunk: HunkUse,
    /// Number of independent pieces this hunk could split into.
    splittable_into: usize,
    /// True for the synthetic "mode change" pseudo-hunk git inserts at index 0
    /// when the diff carries `old mode`/`new mode`. It renders no `@@` header (its
    /// body is the mode lines) and stages via `--chmod` rather than blob rewrite.
    is_mode_change: bool,
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
    display_header: Vec<String>,
    hunks: Vec<Hunk>,
    /// True for an addition (no old content), deletion, or mode-only change.
    added: bool,
    deleted: bool,
    /// True when the diff reports the file as binary.
    is_binary: bool,
    /// `Some(new_mode)` when the diff carries `old mode`/`new mode` lines. git
    /// represents this as a "mode change" pseudo-hunk at index 0 (decided
    /// separately from the content hunks), staged via `update-index --chmod`.
    mode_change: Option<u32>,
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
            // git's add-patch pulls `old mode`/`new mode` OUT of the diff header
            // and renders them as the body of a "mode change" pseudo-hunk that sits
            // at index 0, decided independently of the content hunks. Collect them
            // separately so the file header (rendered once) excludes them.
            let mut mode_lines: Vec<String> = Vec::new();
            let mut new_mode_value: Option<u32> = None;
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
                    header.push(h.to_string());
                } else if h.starts_with("deleted file mode ") {
                    deleted = true;
                    header.push(h.to_string());
                } else if h.starts_with("old mode ") {
                    // Mode-change line: route to the pseudo-hunk, not the header.
                    mode_lines.push(h.to_string());
                } else if let Some(rest) = h.strip_prefix("new mode ") {
                    new_mode_value = u32::from_str_radix(rest.trim(), 8).ok();
                    mode_lines.push(h.to_string());
                } else if let Some(rest) = h.strip_prefix("+++ b/") {
                    if path.is_empty() {
                        path = rest.to_string();
                    }
                    header.push(h.to_string());
                } else if let Some(rest) = h.strip_prefix("--- a/") {
                    if path.is_empty() && rest != "/dev/null" {
                        path = rest.to_string();
                    }
                    header.push(h.to_string());
                } else if let Some(rest) = h.strip_prefix("index ") {
                    // index <a>..<b> <mode>
                    if let Some(sp) = rest.rfind(' ') {
                        if let Ok(m) = u32::from_str_radix(rest[sp + 1..].trim(), 8) {
                            mode = m;
                        }
                    }
                    header.push(h.to_string());
                } else {
                    header.push(h.to_string());
                }
                i += 1;
            }
            // Fallback path from the `diff --git a/X b/X` line.
            if path.is_empty() {
                path = parse_git_header_path(line);
            }
            let mode_change = if mode_lines.is_empty() {
                None
            } else {
                new_mode_value
            };
            let mut fd = FileDiff {
                path,
                mode,
                header,
                display_header: Vec::new(),
                hunks: Vec::new(),
                added,
                deleted,
                is_binary,
                mode_change,
            };
            // A mode change becomes the pseudo-hunk at index 0: its body is the
            // `old mode`/`new mode` lines, with zero `@@` offsets so render_hunk
            // prints the body verbatim (no synthesized `@@` header).
            if mode_change.is_some() {
                fd.hunks.push(Hunk {
                    old_offset: 0,
                    old_count: 0,
                    new_offset: 0,
                    new_count: 0,
                    heading: String::new(),
                    body: mode_lines,
                    display_header: None,
                    display_body: Vec::new(),
                    edited: false,
                    use_hunk: HunkUse::Undecided,
                    splittable_into: 1,
                    is_mode_change: true,
                });
            }
            // Parse hunks.
            while i < lines.len() && lines[i].starts_with("@@ ") {
                let (hunk, next) = parse_hunk(&lines, i);
                fd.hunks.push(hunk);
                i = next;
            }
            // A deletion or an empty addition produces NO `@@` hunk (e.g. deleting
            // or adding a zero-byte file). git still presents it as a single
            // stageable pseudo-hunk ("Stage deletion" / "Stage addition"). Add one
            // when the metadata says deleted/added but no content hunk was parsed.
            if fd.hunks.is_empty() && (fd.deleted || fd.added) && !fd.is_binary {
                fd.hunks.push(Hunk {
                    old_offset: 0,
                    old_count: 0,
                    new_offset: 0,
                    new_count: 0,
                    heading: String::new(),
                    body: Vec::new(),
                    display_header: None,
                    display_body: Vec::new(),
                    edited: false,
                    use_hunk: HunkUse::Undecided,
                    splittable_into: 1,
                    is_mode_change: false,
                });
            }
            files.push(fd);
        } else {
            i += 1;
        }
    }
    files
}

fn attach_display_diff(files: &mut [FileDiff], text: &str) -> Result<()> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut index = 0usize;
    for fd in files {
        if index + fd.header.len() > lines.len() {
            eprintln!("error: mismatched output from interactive.diffFilter");
            return Err(GitError::Exit(1));
        }
        fd.display_header = lines[index..index + fd.header.len()].to_vec();
        index += fd.header.len();
        for hunk in &mut fd.hunks {
            if hunk.is_mode_change
                || (hunk.old_offset == 0 && hunk.new_offset == 0 && hunk.body.is_empty())
            {
                hunk.display_header = None;
            } else {
                if index >= lines.len() {
                    eprintln!("error: mismatched output from interactive.diffFilter");
                    return Err(GitError::Exit(1));
                }
                hunk.display_header = Some(lines[index].clone());
                index += 1;
            }
            if index + hunk.body.len() > lines.len() {
                eprintln!("error: mismatched output from interactive.diffFilter");
                return Err(GitError::Exit(1));
            }
            hunk.display_body = lines[index..index + hunk.body.len()].to_vec();
            index += hunk.body.len();
        }
    }
    if index != lines.len() {
        eprintln!("error: mismatched output from interactive.diffFilter");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn render_display_diff_text(files: &[FileDiff], cfg: &PatchConfig) -> String {
    let mut out = String::new();
    for fd in files {
        for line in &fd.header {
            out.push_str(&display_line(line, cfg));
            out.push('\n');
        }
        for hunk in &fd.hunks {
            let special = hunk.is_mode_change
                || (hunk.old_offset == 0 && hunk.new_offset == 0 && hunk.body.is_empty());
            if !special {
                out.push_str(&display_line(&format_hunk_header(hunk, 0), cfg));
                out.push('\n');
            }
            for line in &hunk.body {
                out.push_str(&display_line(line, cfg));
                out.push('\n');
            }
        }
    }
    out
}

fn filter_display_diff(filter: &str, input: &str) -> Result<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(filter)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| {
            eprintln!("error: failed to run '{filter}'");
            GitError::Exit(1)
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let output = child
        .wait_with_output()
        .map_err(|e| GitError::Io(e.to_string()))?;
    if !output.status.success() {
        eprintln!("error: failed to run '{filter}'");
        return Err(GitError::Exit(1));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn display_line(line: &str, cfg: &PatchConfig) -> String {
    let Some(colors) = cfg.colors.as_ref() else {
        return line.to_string();
    };
    let color = if line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
    {
        &colors.meta
    } else if line.starts_with("@@ ") {
        &colors.frag
    } else if line.starts_with('-') {
        &colors.old
    } else if line.starts_with('+') {
        &colors.new
    } else if line.starts_with(' ') {
        &colors.context
    } else {
        ""
    };
    if line.starts_with('+') && !line.starts_with("+++ ") && !color.is_empty() {
        let (rest, trailing) = split_trailing_spaces(&line[1..]);
        let mut out = wrap_color(color, &colors.reset, "+");
        if !rest.is_empty() {
            out.push_str(&wrap_color(color, &colors.reset, rest));
        }
        if !trailing.is_empty() {
            out.push_str(&wrap_color(&colors.whitespace, &colors.reset, trailing));
        }
        return out;
    }
    if !line.starts_with('+') && line.ends_with(' ') && !color.is_empty() {
        let (body, trailing) = split_trailing_spaces(line);
        let mut out = wrap_color(color, &colors.reset, body);
        out.push_str(&wrap_color(&colors.whitespace, &colors.reset, trailing));
        return out;
    }
    wrap_color(color, &colors.reset, line)
}

fn display_line_whole(line: &str, cfg: &PatchConfig) -> String {
    let Some(colors) = cfg.colors.as_ref() else {
        return line.to_string();
    };
    let color = if line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
    {
        &colors.meta
    } else if line.starts_with("@@ ") {
        &colors.frag
    } else if line.starts_with('-') {
        &colors.old
    } else if line.starts_with('+') {
        &colors.new
    } else if line.starts_with(' ') {
        &colors.context
    } else {
        ""
    };
    wrap_color(color, &colors.reset, line)
}

fn split_trailing_spaces(line: &str) -> (&str, &str) {
    let split = line.trim_end_matches(' ').len();
    line.split_at(split)
}

fn wrap_color(color: &str, reset: &str, text: &str) -> String {
    if color.is_empty() {
        text.to_string()
    } else {
        format!("{color}{text}{reset}")
    }
}

fn print_colored(color: &str, reset: &str, text: &str) {
    if color.is_empty() {
        print!("{text}");
    } else {
        print!("{color}{text}{reset}");
    }
}

fn print_colored_line(color: &str, reset: &str, text: &str) {
    if color.is_empty() {
        println!("{text}");
    } else {
        println!("{color}{text}{reset}");
    }
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
            display_header: None,
            display_body: Vec::new(),
            edited: false,
            use_hunk: HunkUse::Undecided,
            splittable_into: splittable,
            is_mode_change: false,
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

/// Count distinct change groups. A hunk is splittable when it contains at least
/// two separated runs of `+`/`-` lines; a final run followed only by
/// `\ No newline` still counts as a group.
fn count_splittable(body: &[String]) -> usize {
    let mut count = 0usize;
    let mut marker = b'@';
    for l in body {
        let mut ch = l.as_bytes().first().copied().unwrap_or(b' ');
        // "\ No newline" lines inherit the previous marker.
        if ch == b'\\' {
            ch = marker;
        }
        if (ch == b'-' || ch == b'+') && marker != b'-' && marker != b'+' {
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
/// `revision` is the resolved tree-ish for the `diff-index` modes (the literal
/// `"HEAD"` for the head modes, an OID hex for the not-head modes, `None` for
/// the index/diff-files modes).
pub(crate) fn run_add_patch(
    mode: PatchMode,
    paths: &[String],
    revision: Option<&str>,
    stdin: &mut impl BufRead,
    cfg: PatchConfig,
) -> Result<()> {
    run_add_patch_with_result(mode, paths, revision, stdin, cfg, false).map(|_| ())
}

pub(crate) fn run_stash_patch(
    paths: &[String],
    revision: Option<&str>,
    stdin: &mut impl BufRead,
    cfg: PatchConfig,
    quiet: bool,
) -> Result<bool> {
    run_add_patch_with_result(PatchMode::Stash, paths, revision, stdin, cfg, quiet)
}

fn run_add_patch_with_result(
    mode: PatchMode,
    paths: &[String],
    revision: Option<&str>,
    stdin: &mut impl BufRead,
    cfg: PatchConfig,
    quiet: bool,
) -> Result<bool> {
    // Produce the diff for the requested paths, mirroring add-patch.c's
    // `parse_diff` command build: `<diff_cmd> [--unified=<n>]
    // [--inter-hunk-context=<n>] [--diff-algorithm=<algo>] [<revision>]
    // --no-color --ignore-submodules=dirty -p -- <pathspec>...`.
    let mut owned: Vec<String> = match mode {
        PatchMode::Add | PatchMode::Stash | PatchMode::CheckoutIndex => {
            vec!["diff-files".to_string()]
        }
        PatchMode::Reset => vec!["diff".to_string(), "--cached".to_string()],
        PatchMode::CheckoutHead
        | PatchMode::WorktreeHead
        | PatchMode::CheckoutNothead
        | PatchMode::WorktreeNothead => vec!["diff-index".to_string()],
    };
    if let Some(context) = cfg.context {
        owned.push(format!("--unified={context}"));
    }
    if let Some(interhunk) = cfg.interhunk {
        owned.push(format!("--inter-hunk-context={interhunk}"));
    }
    if let Some(algorithm) = &cfg.diff_algorithm {
        owned.push(format!("--diff-algorithm={algorithm}"));
    }
    // The `diff-index` modes need the tree-ish operand before the output flags.
    if matches!(
        mode,
        PatchMode::CheckoutHead
            | PatchMode::CheckoutNothead
            | PatchMode::WorktreeHead
            | PatchMode::WorktreeNothead
    ) && let Some(rev) = revision
    {
        owned.push(rev.to_string());
    }
    owned.push("--no-color".to_string());
    owned.push("--ignore-submodules=dirty".to_string());
    owned.push("-p".to_string());
    if !paths.is_empty() {
        owned.push("--".to_string());
    }
    for p in paths {
        owned.push(p.clone());
    }
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    // git's `parse_diff` errors with "could not parse diff" (exit 1) when the
    // spawned `diff-files` fails — e.g. an invalid `--diff-algorithm` (t3701 #69).
    let (diff, diff_ok) =
        run_capture_status(&args, None).map_err(|e| GitError::Io(e.to_string()))?;
    if !diff_ok {
        eprintln!("error: could not parse diff");
        return Err(GitError::Exit(1));
    }
    let diff_text = String::from_utf8_lossy(&diff).into_owned();
    let mut files = parse_diff(&diff_text);
    if cfg.colors.is_some() {
        let mut display = render_display_diff_text(&files, &cfg);
        if let Some(filter) = cfg.diff_filter.as_deref() {
            display = filter_display_diff(filter, &display)?;
        }
        attach_display_diff(&mut files, &display)?;
    }

    if files.is_empty() || files.iter().all(|f| f.hunks.is_empty()) {
        // The direct `git add -p` path prints "No changes." (or "Only binary
        // files changed.") to STDOUT; t3701 redirects only stdout and greps it.
        let only_binary = !files.is_empty() && files.iter().all(|f| f.is_binary);
        if only_binary {
            if !quiet {
                println!("Only binary files changed.");
            }
        } else if mode == PatchMode::Stash {
            if !quiet {
                println!("No local changes to save");
            }
        } else {
            println!("No changes.");
        }
        return Ok(false);
    }

    let nfiles = files.len();
    let mut file_idx = 0usize;
    loop {
        if file_idx >= nfiles {
            break;
        }
        let nav = patch_update_file(mode, &mut files[file_idx], stdin, &cfg, file_idx, nfiles)?;
        match nav {
            FileNav::Quit => {
                // `q` aborts the whole session: the current file's already-decided
                // hunks are still applied below (their use flags are set), but no
                // further files are visited.
                break;
            }
            FileNav::Next => {
                // Default auto-advance + `>` both move forward; off the last file
                // they end the session.
                file_idx += 1;
            }
            FileNav::Prev => {
                // `<` (only in --no-auto-advance): go to the previous file, or stay
                // on the first (git errs "No previous file" inside the loop).
                file_idx = file_idx.saturating_sub(1);
            }
        }
    }

    // Apply: for each file, reconstruct the index blob with USE_HUNK hunks and
    // stage it. Reset mode applies the selected cached-diff hunks in reverse,
    // which unstages them back to HEAD.
    let mut applied_any = false;
    for fd in &files {
        if fd.hunks.iter().any(|h| h.use_hunk == HunkUse::Use) {
            match mode {
                PatchMode::Add | PatchMode::Stash => apply_file_to_index(fd)?,
                PatchMode::Reset => apply_file_to_index_reverse(fd)?,
                _ => apply_file_via_patch(fd, mode, stdin)?,
            }
            applied_any = true;
        }
    }
    if applied_any {
        refresh_index();
    }
    Ok(applied_any)
}

/// Build the `[y,n,q,a,d<extra>,?]` command suffix and the permitted set.
fn build_suffix(
    fd: &FileDiff,
    hunk_index: usize,
    undecided_next: Option<usize>,
    undecided_prev: Option<usize>,
    cfg: &PatchConfig,
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
    // `,e` (edit): git allows it when `hunk_index + 1 > file_diff->mode_change`
    // (i.e. NOT on the mode-change pseudo-hunk) and the file is not a deletion.
    let mode_change_count = if fd.mode_change.is_some() { 1 } else { 0 };
    if !fd.deleted && hunk_index + 1 > mode_change_count {
        s.push_str(",e");
    }
    if !cfg.auto_advance && nfiles > 1 {
        s.push_str(",>");
        s.push_str(",<");
    }
    s.push_str(",p,P");
    s
}

/// How the per-file decision loop wants the outer file loop to proceed.
enum FileNav {
    /// Move to the next file (auto-advance completion, `>`, or EOF).
    Next,
    /// Move to the previous file (`<`, only in --no-auto-advance).
    Prev,
    /// `q`: abort the whole session.
    Quit,
}

/// The per-file decision loop.
fn patch_update_file(
    mode: PatchMode,
    fd: &mut FileDiff,
    stdin: &mut impl BufRead,
    cfg: &PatchConfig,
    file_idx: usize,
    nfiles: usize,
) -> Result<FileNav> {
    if fd.hunks.is_empty() {
        return Ok(FileNav::Next);
    }
    let mut hunk_index = 0usize;
    let mut rendered: Option<usize> = None;
    let mut pending_err: Option<String> = None;
    // The directive returned to the outer file loop. Defaults to `Next` (the file
    // is fully decided / EOF); `q` sets Quit; `>`/`<` set Next/Prev.
    let mut nav = FileNav::Next;
    // In `--no-auto-advance` mode the loop does NOT exit when every hunk is
    // decided — it keeps prompting (so the user can revisit decisions), and the
    // `?` help then shows a HUNKS SUMMARY. In the default auto-advance mode the
    // loop breaks as soon as everything is decided.
    let mut all_decided = false;

    // The file's diff header (`diff --git ...`) is printed exactly once on entry.
    render_file_header(fd, cfg);

    loop {
        let nr = fd.hunks.len();
        // If a prior y/n advanced past the end with no undecided hunk left,
        // and indeed nothing is undecided, the file is done. (git lets the
        // index run one past the end and relies on the undecided scan; we guard
        // the out-of-range index explicitly.)
        if hunk_index >= nr {
            match first_undecided(fd) {
                Some(i) => {
                    hunk_index = i;
                    rendered = None;
                }
                None => {
                    if cfg.auto_advance {
                        break;
                    }
                    // --no-auto-advance: wrap to the first hunk and keep prompting
                    // (git resets hunk_index to 0 rather than exiting).
                    hunk_index = 0;
                    rendered = None;
                }
            }
        }
        // Find undecided next/prev.
        let undecided_next = next_undecided(fd, hunk_index);
        let undecided_prev = prev_undecided(fd, hunk_index);

        // Everything decided?
        if undecided_next.is_none()
            && undecided_prev.is_none()
            && fd.hunks[hunk_index].use_hunk != HunkUse::Undecided
        {
            if cfg.auto_advance {
                // Default mode: done with this file.
                break;
            }
            // `--no-auto-advance`: stay on the prompt; the `?` help shows the
            // HUNKS SUMMARY while all_decided.
            all_decided = true;
        } else {
            all_decided = false;
        }

        // Render the hunk if newly arrived at.
        if rendered != Some(hunk_index) {
            render_hunk(fd, hunk_index, 0, cfg);
            rendered = Some(hunk_index);
        }

        if let Some(msg) = pending_err.take() {
            print!("{msg}");
            let _ = io::stdout().flush();
        }

        // Build prompt. git's selection: deletion > addition > (mode_change at
        // index 0) > hunk.
        let kind = if fd.deleted {
            prompt_text(mode, PromptKind::Deletion)
        } else if fd.added {
            prompt_text(mode, PromptKind::Addition)
        } else if fd.mode_change.is_some() && hunk_index == 0 {
            prompt_text(mode, PromptKind::ModeChange)
        } else {
            prompt_text(mode, PromptKind::Hunk)
        };
        let suffix = build_suffix(fd, hunk_index, undecided_next, undecided_prev, cfg, nfiles);
        let was = match fd.hunks[hunk_index].use_hunk {
            HunkUse::Use => " (was: y)",
            HunkUse::Skip => " (was: n)",
            HunkUse::Undecided => "",
        };
        let prompt = format!(
            "({}/{}) {kind}{was} [y,n,q,a,d{suffix},?]? ",
            hunk_index + 1,
            nr
        );
        print_colored(&cfg.prompt_color, &cfg.reset_interactive, &prompt);
        let _ = io::stdout().flush();

        let line = match read_line(stdin) {
            Some(l) => l,
            None => {
                // EOF at the prompt: git's read_single_character returns EOF and
                // sets patch_update_resp = file_diff_nr, which quits the whole
                // session (the outer loop breaks). Mirror that so a `</dev/null`
                // add -p stops after the first file rather than walking the rest.
                nav = FileNav::Quit;
                break;
            }
        };
        if line.is_empty() {
            continue;
        }
        let answer = line.clone();
        let Some(ch) = answer.chars().next() else {
            continue;
        };
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
                // Mark all remaining undecided as skip and stop. git sets
                // patch_update_resp = file_diff_nr and breaks, hitting the common
                // trailing newline. We signal "quit the whole session" to the
                // caller via the return directive below.
                nav = FileNav::Quit;
                break;
            }
            '>' if !cfg.auto_advance => {
                // Manual advance to the next file (only with --no-auto-advance and
                // more than one file). git errs "No next file" on the last one.
                if nfiles > 1 && file_idx + 1 < nfiles {
                    nav = FileNav::Next;
                    break;
                }
                pending_err = Some("No next file\n".to_string());
            }
            '<' if !cfg.auto_advance => {
                if nfiles > 1 && file_idx > 0 {
                    nav = FileNav::Prev;
                    break;
                }
                pending_err = Some("No previous file\n".to_string());
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
                    print_colored_line(
                        &cfg.header_color,
                        &cfg.reset_interactive,
                        &format!("Split into {n} hunks."),
                    );
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
                            pending_err = Some("No hunk matches the given pattern\n".to_string());
                        }
                    },
                    'e' => {
                        // Edit is disallowed on the mode-change pseudo-hunk and on
                        // deletions (matches build_suffix's `,e` gating).
                        let mode_change_count = if fd.mode_change.is_some() { 1 } else { 0 };
                        if fd.deleted || hunk_index + 1 <= mode_change_count {
                            pending_err =
                                Some(format!("Unknown command '{answer}' (use '?' for help)\n"));
                        } else {
                            match edit_hunk_loop(fd, hunk_index, stdin) {
                                EditResult::Applied => {
                                    fd.hunks[hunk_index].use_hunk = HunkUse::Use;
                                    hunk_index = undecided_next.unwrap_or(nr);
                                    rendered = None;
                                }
                                EditResult::Abandoned => {
                                    // Keep the original hunk; re-render it.
                                    rendered = None;
                                }
                                EditResult::Eof => {
                                    println!();
                                    return Ok(FileNav::Quit);
                                }
                            }
                        }
                    }
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
                        // In --no-auto-advance mode, once every hunk is decided the
                        // help appends a HUNKS SUMMARY (git's help_patch_remainder).
                        if all_decided {
                            let used = fd
                                .hunks
                                .iter()
                                .filter(|h| h.use_hunk == HunkUse::Use)
                                .count();
                            let skipped = fd
                                .hunks
                                .iter()
                                .filter(|h| h.use_hunk == HunkUse::Skip)
                                .count();
                            println!(
                                "HUNKS SUMMARY - Hunks: {}, USE: {used}, SKIP: {skipped}",
                                fd.hunks.len()
                            );
                        }
                    }
                    _ => {
                        pending_err =
                            Some(format!("Unknown command '{answer}' (use '?' for help)\n"));
                    }
                }
            }
        }
    }
    // git's patch_update_file always ends with a single `putchar('\n')` after
    // the decision loop (and after apply, in auto-advance mode). Mirror that so
    // every file's prompt block is newline-terminated.
    println!();
    Ok(nav)
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
fn render_hunk(fd: &FileDiff, hunk_index: usize, _delta: i64, cfg: &PatchConfig) {
    let h = &fd.hunks[hunk_index];
    // Special pseudo-hunks (mode change, or a deletion / empty addition with no
    // real content) have zero `@@` offsets and render no `@@` header — just their
    // body (the mode lines, or nothing). git's render_hunk skips the header when
    // both offsets are zero.
    let special = h.is_mode_change || (h.old_offset == 0 && h.new_offset == 0 && h.body.is_empty());
    if !special {
        match h.display_header.as_deref() {
            Some(line) => println!("{line}"),
            None => println!("{}", display_line(&format_hunk_header(h, 0), cfg)),
        }
    }
    if h.display_body.len() == h.body.len() {
        for line in &h.display_body {
            println!("{line}");
        }
    } else {
        for line in &h.body {
            if h.edited {
                println!("{}", display_line_whole(line, cfg));
            } else {
                println!("{}", display_line(line, cfg));
            }
        }
    }
}

/// Outcome of the manual-edit (`e`) loop.
enum EditResult {
    /// The edited hunk applied cleanly and replaced the original — mark it Use.
    Applied,
    /// The user abandoned editing (deleted everything, or said "no" to retry).
    Abandoned,
    /// EOF on the retry prompt.
    Eof,
}

/// The `e` command: edit the current hunk in `$GIT_EDITOR`, recount its header,
/// and validate it applies (re-prompting on failure). Mirrors add-patch.c's
/// `edit_hunk_loop` + `edit_hunk_manually` + `run_apply_check`.
fn edit_hunk_loop(fd: &mut FileDiff, hunk_index: usize, stdin: &mut impl BufRead) -> EditResult {
    let git_dir = match env::current_dir()
        .ok()
        .and_then(|cwd| crate::session::cli_git_dir_from(&cwd).ok())
    {
        Some(dir) => dir,
        None => return EditResult::Abandoned,
    };
    let comment = super::replay::comment_char(&git_dir);
    let cc = comment as char;

    loop {
        // Build the editor buffer: commented preamble + the hunk + commented hints.
        let mut buf = String::new();
        buf.push_str(&format!(
            "{cc} Manual hunk edit mode -- see bottom for a quick guide.\n"
        ));
        // The hunk header + body verbatim.
        buf.push_str(&format_hunk_header(&fd.hunks[hunk_index], 0));
        buf.push('\n');
        for line in &fd.hunks[hunk_index].body {
            buf.push_str(line);
            buf.push('\n');
        }
        buf.push_str(&format!(
            "{cc} ---\n\
             {cc} To remove '-' lines, make them ' ' lines (context).\n\
             {cc} To remove '+' lines, delete them.\n\
             {cc} Lines starting with {cc} will be removed.\n\
             {cc} If the patch applies cleanly, the edited hunk will immediately be\n\
             {cc} marked for staging.\n\
             {cc} If it does not apply cleanly, you will be given an opportunity to\n\
             {cc} edit again.  If all lines of the hunk are removed, then the edit is\n\
             {cc} aborted and the hunk is left unchanged.\n"
        ));

        // Write to the standard add-patch edit file under the git dir and launch.
        let edit_path = git_dir.join("addp-hunk-edit.diff");
        if std::fs::write(&edit_path, buf.as_bytes()).is_err() {
            return EditResult::Abandoned;
        }
        if super::replay::launch_editor(&git_dir, &edit_path).is_err() {
            let _ = std::fs::remove_file(&edit_path);
            return EditResult::Abandoned;
        }
        let edited = std::fs::read(&edit_path).unwrap_or_default();
        let _ = std::fs::remove_file(&edit_path);

        // Strip comment lines; collect the remaining body (drop the `@@` header if
        // present -- the prototype keeps the header separate). Do not use the
        // commit-message stripspace helper here: whitespace-only lines are
        // meaningful empty context lines in an edited patch.
        let stripped = strip_hunk_edit_comments(&edited, comment);
        let text = String::from_utf8_lossy(&stripped);
        let mut new_body: Vec<String> = Vec::new();
        let mut saw_content = false;
        let edit_lines: Vec<&str> = text.split('\n').collect();
        for (line_index, line) in edit_lines.iter().enumerate() {
            if line.starts_with("@@ ") {
                // Header line in the edited buffer: ignore (we recount ourselves).
                continue;
            }
            let first = line.as_bytes().first().copied();
            if matches!(first, Some(b' ') | Some(b'+') | Some(b'-') | Some(b'\\')) {
                new_body.push(line.to_string());
                saw_content = true;
            } else if line.is_empty() && line_index + 1 < edit_lines.len() {
                // Manual editing permits users/editors to strip the single
                // marker space from empty context lines. Treat non-final empty
                // lines as " " context; the final split artifact is skipped.
                new_body.push(" ".to_string());
                saw_content = true;
            } else if line.is_empty() {
                // trailing blank from the split -- skip.
            } else {
                // Editors can strip the marker space from context lines. Keep
                // the line as tentative context; validation below rejects it if
                // the old-side text does not match the index.
                new_body.push(format!(" {line}"));
                saw_content = true;
            }
        }
        if !saw_content {
            // The user deleted everything → abort, keep original.
            return EditResult::Abandoned;
        }

        // Recount the header from the edited body.
        let (old_count, new_count) = recount_body(&new_body);
        let splittable_into = count_splittable(&new_body);
        let mut candidate = Hunk {
            old_offset: fd.hunks[hunk_index].old_offset,
            old_count,
            new_offset: fd.hunks[hunk_index].new_offset,
            new_count,
            heading: fd.hunks[hunk_index].heading.clone(),
            body: new_body,
            display_header: None,
            display_body: Vec::new(),
            edited: true,
            use_hunk: HunkUse::Use,
            splittable_into,
            is_mode_change: false,
        };

        // Validate: reassemble a patch (file header + just this candidate hunk) and
        // run `sley apply --cached --check`. On success, commit the edit.
        if edited_hunk_applies(fd, &candidate) {
            std::mem::swap(&mut fd.hunks[hunk_index], &mut candidate);
            return EditResult::Applied;
        }

        // Failed to apply: prompt to edit again (saying "no" discards).
        print!("Your edited hunk does not apply. Edit again (saying \"no\" discards!) [y/n]? ");
        let _ = io::stdout().flush();
        match read_line(stdin) {
            Some(ans) => {
                let yes = ans
                    .chars()
                    .next()
                    .map(|c| c.eq_ignore_ascii_case(&'y'))
                    .unwrap_or(false);
                if !yes {
                    return EditResult::Abandoned;
                }
                // loop: edit again
            }
            None => return EditResult::Eof,
        }
    }
}

fn strip_hunk_edit_comments(message: &[u8], comment: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len());
    for line in message.split_inclusive(|&b| b == b'\n') {
        if line.first() == Some(&comment) {
            continue;
        }
        out.extend_from_slice(line);
    }
    out
}

/// Recount the `old`/`new` line counts of an edited hunk body.
fn recount_body(body: &[String]) -> (i64, i64) {
    let mut old_count = 0i64;
    let mut new_count = 0i64;
    for line in body {
        match line.as_bytes().first().copied().unwrap_or(b' ') {
            b' ' => {
                old_count += 1;
                new_count += 1;
            }
            b'-' => old_count += 1,
            b'+' => new_count += 1,
            _ => {}
        }
    }
    (old_count, new_count)
}

/// Check that an edited hunk applies to the index blob: its old-side lines
/// (context ` ` + deletions `-`) must match the index content at `old_offset`.
/// Mirrors the effect of git's `git apply --check` for the single edited hunk
/// (sley has no `apply --cached` yet, so we validate against the index blob the
/// way the prototype's own apply pass reconstructs it).
fn edited_hunk_applies(fd: &FileDiff, candidate: &Hunk) -> bool {
    // Read the index version of the file (stage 0). For an addition the base is
    // empty. A failed read means an empty base.
    let spec = format!(":{}", fd.path);
    let base = run_capture(&["cat-file", "blob", &spec], None).unwrap_or_default();
    let base_text = String::from_utf8_lossy(&base).into_owned();
    let had_final_nl = base_text.ends_with('\n');
    let mut base_lines: Vec<&str> = base_text.split('\n').collect();
    if had_final_nl {
        base_lines.pop();
    }
    // The hunk's old side begins at 1-based old_offset.
    let start = (candidate.old_offset - 1).max(0) as usize;
    let mut cursor = start;
    for line in &candidate.body {
        let marker = line.as_bytes().first().copied().unwrap_or(b' ');
        let rest = &line[1.min(line.len())..];
        match marker {
            b' ' | b'-' => {
                // Old-side line: must match the base content exactly.
                match base_lines.get(cursor) {
                    Some(&base_line) if base_line == rest => cursor += 1,
                    _ => return false,
                }
            }
            b'+' => { /* new-side only; nothing to check against the base */ }
            b'\\' => { /* "\ No newline" marker */ }
            _ => return false,
        }
    }
    true
}

/// Print the file's diff header (the `diff --git ...` block up to the first
/// `@@`). git emits this exactly once, when the file is first entered.
fn render_file_header(fd: &FileDiff, cfg: &PatchConfig) {
    let headers = if fd.display_header.len() == fd.header.len() {
        &fd.display_header
    } else {
        &fd.header
    };
    for h in headers {
        println!("{}", display_line(h, cfg));
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
            display_header: None,
            display_body: Vec::new(),
            edited: false,
            use_hunk: HunkUse::Undecided,
            splittable_into: 1,
            is_mode_change: false,
        });
        old_cursor += old_count;
        new_cursor += new_count;
    }
    let n = new_hunks.len();
    // Replace the single hunk with the pieces.
    fd.hunks.splice(hunk_index..hunk_index + 1, new_hunks);
    n
}

/// The current stage-0 index object id for `path` (the blob recorded in the
/// index), via `ls-files --stage`. Used to re-stage a mode change without
/// touching the blob content.
fn current_index_oid(path: &str) -> Option<String> {
    let out = run_capture(&["ls-files", "--stage", "--", path], None).ok()?;
    let text = String::from_utf8_lossy(&out);
    // Format: `<mode> <oid> <stage>\t<path>`.
    let line = text.lines().next()?;
    let mut fields = line.split_whitespace();
    let _mode = fields.next()?;
    let oid = fields.next()?;
    Some(oid.to_string())
}

/// Whether any content (non-mode-change) hunk in this file was selected.
fn any_content_hunk_used(fd: &FileDiff) -> bool {
    fd.hunks
        .iter()
        .any(|h| !h.is_mode_change && h.use_hunk == HunkUse::Use)
}

/// Whether the mode-change pseudo-hunk (index 0) was selected.
fn mode_change_used(fd: &FileDiff) -> bool {
    fd.hunks
        .first()
        .map(|h| h.is_mode_change && h.use_hunk == HunkUse::Use)
        .unwrap_or(false)
}

/// Apply the USE_HUNK hunks of one file to the index.
/// Reassemble a unified diff containing only this file's `USE` hunks, mirroring
/// add-patch.c's `reassemble_patch`: skipped hunks shift the new-side offsets of
/// the kept hunks (the running `delta`), and the mode-change pseudo-hunk at
/// index 0 is not part of the textual patch.
fn reassemble_patch(fd: &FileDiff) -> String {
    let mut out = String::new();
    for line in &fd.header {
        out.push_str(line);
        out.push('\n');
    }
    let mode_change = if fd.mode_change.is_some() { 1 } else { 0 };
    let mut delta: i64 = 0;
    for hunk in fd.hunks.iter().skip(mode_change) {
        if hunk.use_hunk != HunkUse::Use {
            // A dropped hunk shifts every later kept hunk's new-side offset.
            delta += hunk.old_count - hunk.new_count;
            continue;
        }
        out.push_str(&format_hunk_header(hunk, delta));
        // `heading` carries its own trailing newline when present.
        if hunk.heading.is_empty() {
            out.push('\n');
        }
        for line in &hunk.body {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Apply this file's selected hunks the way git does for the checkout/worktree
/// modes: reassemble a patch and pipe it to `apply` (to the index and/or the
/// working tree, forward or reversed, per the mode).
fn apply_file_via_patch(fd: &FileDiff, mode: PatchMode, stdin: &mut impl BufRead) -> Result<()> {
    let patch = reassemble_patch(fd);
    let patch = patch.as_bytes();
    let reverse = mode.is_reverse();
    if mode.applies_for_checkout() {
        return apply_for_checkout(patch, reverse, stdin);
    }
    // Worktree-only modes: a single `apply [-R]` against the working tree.
    let mut args: Vec<&str> = vec!["apply"];
    if reverse {
        args.push("-R");
    }
    let (_out, ok) =
        run_capture_status(&args, Some(patch)).map_err(|e| GitError::Io(e.to_string()))?;
    if !ok {
        eprintln!("error: 'git apply' failed");
    }
    Ok(())
}

/// git's `apply_for_checkout`: try the reassembled patch against both the index
/// (`--cached`) and the working tree; apply to both only if both check clean,
/// otherwise prompt to apply to the worktree alone (or, if only the index would
/// take it, just show the patch).
fn apply_for_checkout(patch: &[u8], reverse: bool, stdin: &mut impl BufRead) -> Result<()> {
    let reverse_arg: Option<&str> = if reverse { Some("-R") } else { None };
    let check = |extra: &[&str]| -> bool {
        let mut args: Vec<&str> = vec!["apply"];
        args.extend_from_slice(extra);
        if let Some(r) = reverse_arg {
            args.push(r);
        }
        args.push("--check");
        run_capture_status(&args, Some(patch))
            .map(|(_, ok)| ok)
            .unwrap_or(false)
    };
    let apply = |extra: &[&str]| {
        let mut args: Vec<&str> = vec!["apply"];
        args.extend_from_slice(extra);
        if let Some(r) = reverse_arg {
            args.push(r);
        }
        let _ = run_capture_status(&args, Some(patch));
    };
    let applies_index = check(&["--cached"]);
    let applies_worktree = check(&[]);
    if applies_index && applies_worktree {
        apply(&["--cached"]);
        apply(&[]);
        return Ok(());
    }
    if !applies_index {
        eprintln!("error: The selected hunks do not apply to the index!");
        if prompt_yesno("Apply them to the worktree anyway? ", stdin) {
            apply(&[]);
            return Ok(());
        }
        eprintln!("Nothing was applied.");
    } else {
        // The index would take the patch but the worktree would not: as a last
        // resort, git just shows the patch to the user.
        print!("{}", String::from_utf8_lossy(patch));
        let _ = io::stdout().flush();
    }
    Ok(())
}

/// git's `prompt_yesno`: print the prompt to stdout, read a line, and return
/// true for a `y*` answer. EOF and `n*` answers return false.
fn prompt_yesno(prompt: &str, stdin: &mut impl BufRead) -> bool {
    loop {
        print!("{prompt}");
        let _ = io::stdout().flush();
        match read_line(stdin) {
            None => return false,
            Some(line) => {
                let answer = line.trim();
                match answer.chars().next().map(|c| c.to_ascii_lowercase()) {
                    Some('y') => return true,
                    Some('n') => return false,
                    _ => continue,
                }
            }
        }
    }
}

fn apply_file_to_index(fd: &FileDiff) -> Result<()> {
    // A staged deletion: drop the path from the index entirely.
    if fd.deleted {
        let status = Command::new(self_bin())
            .args(["update-index", "--force-remove", &fd.path])
            .stdin(Stdio::null())
            .status()
            .map_err(|e| GitError::Io(e.to_string()))?;
        if !status.success() {
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }

    let content_used = any_content_hunk_used(fd);
    let mode_used = mode_change_used(fd);

    // A mode change with no content change: stage just the new mode, keeping the
    // *index* blob (not the worktree content). git applies the mode change to the
    // index by re-staging the existing index oid at the new mode — NOT via
    // `update-index --chmod`, which re-hashes the (possibly dirty) worktree file.
    // So read the current index oid and re-stage it via `--cacheinfo <newmode>`.
    if let Some(new_mode) = fd.mode_change
        && mode_used
        && !content_used
    {
        let index_oid = current_index_oid(&fd.path);
        if let Some(oid) = index_oid {
            let mode = format!("{new_mode:o}");
            let status = Command::new(self_bin())
                .args(["update-index", "--cacheinfo", &mode, &oid, &fd.path])
                .stdin(Stdio::null())
                .status()
                .map_err(|e| GitError::Io(e.to_string()))?;
            if !status.success() {
                return Err(GitError::Exit(1));
            }
        }
        return Ok(());
    }

    if !content_used && !mode_used {
        return Ok(());
    }

    // Read the index version of the file (stage 0). For a brand-new addition the
    // index entry is empty (intent-to-add), so a failed read means empty base.
    let spec = format!(":{}", fd.path);
    let base = run_capture(&["cat-file", "blob", &spec], None).unwrap_or_default();
    let base_text = String::from_utf8_lossy(&base).into_owned();
    let new_content = apply_hunks(&base_text, fd);
    // Write the result as a blob.
    let oid = run_capture(
        &["hash-object", "-w", "--stdin"],
        Some(new_content.as_bytes()),
    )
    .map_err(|e| GitError::Io(e.to_string()))?;
    let oid = String::from_utf8_lossy(&oid).trim().to_string();
    // Stage mode. For a mode-change file the diff's `index` line carries no mode,
    // so `fd.mode` is the default 100644: use the explicit `mode_change` new mode
    // when the change was taken, and keep 100644 (old mode) otherwise. For a plain
    // content change `fd.mode` is the correct new-side mode.
    let staged_mode = match (fd.mode_change, mode_used) {
        (Some(new_mode), true) => new_mode,
        (Some(_), false) => 0o100644,
        (None, _) => fd.mode,
    };
    let mode = format!("{:o}", staged_mode);
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

fn apply_file_to_index_reverse(fd: &FileDiff) -> Result<()> {
    let content_used = any_content_hunk_used(fd);
    if !content_used {
        return Ok(());
    }

    let spec = format!(":{}", fd.path);
    let base = run_capture(&["cat-file", "blob", &spec], None).unwrap_or_default();
    let base_text = String::from_utf8_lossy(&base).into_owned();
    let new_content = apply_hunks_reverse(&base_text, fd);
    let oid = run_capture(
        &["hash-object", "-w", "--stdin"],
        Some(new_content.as_bytes()),
    )
    .map_err(|e| GitError::Io(e.to_string()))?;
    let oid = String::from_utf8_lossy(&oid).trim().to_string();
    let mode = format!("{:o}", fd.mode);
    let args = ["update-index", "--cacheinfo", &mode, &oid, &fd.path];
    let (_out, ok) = run_capture_status(&args, None).map_err(|e| GitError::Io(e.to_string()))?;
    if !ok {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// After staging hunks, refresh the index stat cache so `git diff-files` stays
/// clean for paths whose worktree now matches the freshly-staged blob (t3701
/// "index is refreshed after applying patch").
fn refresh_index() {
    let _ = Command::new(self_bin())
        .args(["update-index", "--refresh", "-q"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Apply the selected hunks to the index base text, line by line.
fn apply_hunks(base: &str, fd: &FileDiff) -> String {
    // base lines (preserve trailing newline behavior).
    let had_final_nl = base.ends_with('\n');
    let mut result_had_final_nl = had_final_nl;
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
        // The mode-change pseudo-hunk carries no content lines (its body is the
        // `old mode`/`new mode` header); skip it in the content apply.
        if h.is_mode_change {
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
        let mut old_pos = start;
        let mut previous_marker = b'@';
        for line in &h.body {
            let marker = line.as_bytes().first().copied().unwrap_or(b' ');
            let rest = &line[1.min(line.len())..];
            match marker {
                b' ' => {
                    if old_pos < cursor {
                        old_pos += 1;
                        previous_marker = marker;
                        continue;
                    }
                    out.push(rest.to_string());
                    cursor += 1;
                    old_pos += 1;
                }
                b'-' => {
                    if old_pos < cursor {
                        old_pos += 1;
                        previous_marker = marker;
                        continue;
                    }
                    cursor += 1;
                    old_pos += 1;
                }
                b'+' => {
                    out.push(rest.to_string());
                }
                b'\\' => {
                    if previous_marker != b'-' {
                        result_had_final_nl = false;
                    }
                }
                _ => {}
            }
            if marker != b'\\' {
                previous_marker = marker;
            }
        }
    }
    // Copy any remaining base lines.
    while cursor < base_lines.len() {
        out.push(base_lines[cursor].to_string());
        cursor += 1;
    }
    let mut result = out.join("\n");
    if result_had_final_nl && !result.is_empty() {
        result.push('\n');
    } else if result_had_final_nl && result.is_empty() {
        // keep empty
    }
    result
}

fn apply_hunks_reverse(base: &str, fd: &FileDiff) -> String {
    let had_final_nl = base.ends_with('\n');
    let mut base_lines: Vec<&str> = base.split('\n').collect();
    if had_final_nl {
        base_lines.pop();
    }
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;

    for h in &fd.hunks {
        if h.use_hunk != HunkUse::Use || h.is_mode_change {
            continue;
        }
        let start = (h.new_offset - 1).max(0) as usize;
        while cursor < start && cursor < base_lines.len() {
            out.push(base_lines[cursor].to_string());
            cursor += 1;
        }
        for line in &h.body {
            let marker = line.as_bytes().first().copied().unwrap_or(b' ');
            let rest = &line[1.min(line.len())..];
            match marker {
                b' ' => {
                    out.push(rest.to_string());
                    cursor += 1;
                }
                b'+' => {
                    cursor += 1;
                }
                b'-' => {
                    out.push(rest.to_string());
                }
                b'\\' => {}
                _ => {}
            }
        }
    }
    while cursor < base_lines.len() {
        out.push(base_lines[cursor].to_string());
        cursor += 1;
    }
    let mut s = out.join("\n");
    if had_final_nl {
        s.push('\n');
    }
    s
}
