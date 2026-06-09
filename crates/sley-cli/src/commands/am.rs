//! `git am` — apply a series of patches from a mailbox.
//!
//! This implements the common subset of `git am`: it reads an mbox (one or more
//! files, or stdin), parses each message's From/Subject/Date headers plus body
//! and unified diff, applies the diff to the worktree and index, and creates one
//! commit per patch that preserves the original author identity, author date,
//! and commit message. The committer is taken from the environment/config the
//! same way `git commit` does, so applying patches produced by `git format-patch`
//! reproduces the original commit object IDs byte-for-byte.
//!
//! Series state is persisted under `.git/rebase-apply/` using the same file
//! layout real git uses (`next`, `last`, `0001`..`NNNN`, `author-script`,
//! `info`, `final-commit`, `msg`, `patch`, `abort-safety`, …) so `--abort`,
//! `--continue`/`--resolved`, and `--skip` can resume an interrupted run.
//!
//! Command modules pull their shared plumbing from the crate root; the glob
//! import reaches every helper, type, and re-export visible there (a submodule
//! can access its ancestor module's private items), so `discover_git_dir`,
//! `repository_object_format`, `FileObjectDatabase`, `three_way_merge_trees`,
//! and friends are all in scope without re-listing them.
use crate::*;

/// Parsed command-line configuration for a fresh `git am` invocation.
struct AmOptions {
    /// mbox files to read; empty means read stdin.
    mboxes: Vec<String>,
    /// Suppress the per-patch `Applying:` line (`-q`/`--quiet`).
    quiet: bool,
    /// Append a `Signed-off-by` trailer to each commit (`-s`/`--signoff`).
    signoff: bool,
    /// Fall back to a 3-way merge when straight application fails (`-3`).
    three_way: bool,
    /// Keep non-empty commits whose patch is empty rather than erroring.
    keep_non_patch: bool,
}

/// A single message extracted from an mbox: identity, message, and raw diff.
struct AmPatch {
    /// Author name from the `From:` header.
    author_name: String,
    /// Author email from the `From:` header.
    author_email: String,
    /// Author date from the `Date:` header, already normalised to
    /// `"<seconds> <±HHMM>"`. `None` when the header was absent or unparsable
    /// (the committer/env date is then used).
    author_date: Option<String>,
    /// Original `Date:` header text, preserved verbatim for the author-script.
    author_date_raw: Option<String>,
    /// Cleaned subject line (with any `[PATCH …]` prefix stripped).
    subject: String,
    /// Full commit message (subject + blank line + body), newline-terminated.
    message: Vec<u8>,
    /// The unified diff body (everything from the first `diff`/`---` onward).
    diff: Vec<u8>,
}

/// Entry point for `git am`.
pub(crate) fn cmd_am(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let state_dir = git_dir.join("rebase-apply");

    // Resume sub-operations are mutually exclusive and take no mbox arguments.
    let mut resume = None;
    let mut option_args = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--abort" | "--quit" | "--continue" | "-r" | "--resolved" | "--skip" => {
                if let Some(existing) = resume {
                    return am_incompatible_resume_error(existing, arg);
                }
                resume = Some(match arg.as_str() {
                    "-r" | "--resolved" => "--continue",
                    other => other,
                });
            }
            other => option_args.push(other.to_string()),
        }
    }

    if let Some(resume) = resume {
        return match resume {
            "--abort" => am_abort(&git_dir, &worktree_root, format, &state_dir),
            "--quit" => am_quit(&state_dir),
            "--skip" => am_skip(
                &git_dir,
                &common_git_dir,
                &worktree_root,
                format,
                &state_dir,
            ),
            "--continue" => am_continue(
                &git_dir,
                &common_git_dir,
                &worktree_root,
                format,
                &state_dir,
            ),
            _ => Ok(()),
        };
    }

    let options = parse_am_options(&option_args)?;

    // Starting a new run while one is unfinished is an error in git.
    if state_dir.exists() {
        eprintln!(
            "fatal: previous rebase directory {} still exists but mbox given.",
            display_state_dir(&worktree_root, &state_dir)
        );
        return Err(GitError::Exit(128));
    }

    let input = read_am_input(&options.mboxes)?;

    // git treats explicit mbox files and stdin differently. A file must pass
    // patch-format detection: if it does not look like a mailbox, a mail, or a
    // diff, git aborts with "Patch format detection failed." Stdin is assumed to
    // be mbox, so empty stdin is just a silent no-op.
    let from_files = !options.mboxes.is_empty();
    if from_files && !looks_like_patch_input(&input) {
        eprintln!("Patch format detection failed.");
        return Err(GitError::Exit(128));
    }

    let patches = parse_mbox(&input)?;
    // No messages at all (empty/whitespace stdin) — nothing to do.
    if patches.is_empty() {
        return Ok(());
    }

    let refs = FileRefStore::new(&git_dir, format);
    let head_oid = head_commit_oid(&refs)?.ok_or_else(|| {
        eprintln!("fatal: am: cannot apply patches onto an unborn branch");
        GitError::Exit(128)
    })?;

    write_am_state_dir(&state_dir, &patches, &options, &head_oid)?;

    run_am_series(
        &git_dir,
        &common_git_dir,
        &worktree_root,
        format,
        &state_dir,
        1,
    )
}

/// Parse the non-resume flags of `git am`.
fn parse_am_options(args: &[String]) -> Result<AmOptions> {
    let mut options = AmOptions {
        mboxes: Vec::new(),
        quiet: false,
        signoff: false,
        three_way: false,
        keep_non_patch: false,
    };
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            options.mboxes.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-q" | "--quiet" => options.quiet = true,
            "--no-quiet" => options.quiet = false,
            "-s" | "--signoff" => options.signoff = true,
            "--no-signoff" => options.signoff = false,
            "-3" | "--3way" => options.three_way = true,
            "--no-3way" => options.three_way = false,
            "-k" | "--keep" | "--keep-non-patch" => options.keep_non_patch = true,
            // Accepted no-ops: these affect mail parsing / cosmetics we already
            // handle or that do not change the resulting commits for the inputs
            // `git format-patch` produces.
            "-u"
            | "--utf8"
            | "--no-utf8"
            | "-m"
            | "--message-id"
            | "--no-message-id"
            | "-c"
            | "--scissors"
            | "--no-scissors"
            | "--keep-cr"
            | "--no-keep-cr"
            | "--committer-date-is-author-date"
            | "--no-committer-date-is-author-date"
            | "--ignore-date"
            | "--ignore-whitespace"
            | "--no-ignore-whitespace"
            | "--whitespace"
            | "--rerere-autoupdate"
            | "--no-rerere-autoupdate"
            | "--allow-empty"
            | "--empty=drop"
            | "--empty=keep"
            | "--empty=stop" => {}
            value if value.starts_with("--whitespace=") => {}
            value if value.starts_with("--patch-format=") => {}
            value if value.starts_with("--empty=") => {}
            value if value.starts_with("--exclude=") || value.starts_with("--include=") => {}
            value if value.starts_with("--directory=") || value.starts_with("-p") => {}
            value if value.starts_with('-') && value != "-" => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                am_usage();
                return Err(GitError::Exit(129));
            }
            value => options.mboxes.push(value.to_string()),
        }
        index += 1;
    }
    Ok(options)
}

fn am_usage() {
    eprintln!("usage: git am [--signoff] [--keep] [-q | --quiet] [-3 | --3way] [<mbox>...]");
    eprintln!("   or: git am (--continue | --skip | --abort | --quit)");
}

fn am_incompatible_resume_error(existing: &str, new: &str) -> Result<()> {
    eprintln!("fatal: options '{existing}' and '{new}' cannot be used together");
    Err(GitError::Exit(128))
}

/// Read every mbox file (or stdin when none are given) into one buffer.
fn read_am_input(mboxes: &[String]) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    if mboxes.is_empty() {
        io::stdin().read_to_end(&mut input)?;
    } else {
        for mbox in mboxes {
            input.extend_from_slice(&fs::read(mbox)?);
        }
    }
    Ok(input)
}

// ===========================================================================
// mbox parsing
// ===========================================================================

/// Heuristic patch-format detection for explicit mbox files, mirroring what git
/// does before splitting: the content must look like a mailbox (`From `), a mail
/// (a `Header: value` line such as `From:`/`Subject:`/`Date:`), or a diff
/// (`diff --git`, `--- `, `Index:`). Empty/whitespace-only content fails.
fn looks_like_patch_input(input: &[u8]) -> bool {
    for line in split_keep_newline(input) {
        let line = trim_trailing_newline(&line);
        if line.is_empty() {
            continue;
        }
        if line.starts_with(b"From ") || is_diff_start(line) {
            return true;
        }
        // A mail header line: a non-space token, then a colon (e.g. `Subject:`).
        if let Some(colon) = line.iter().position(|byte| *byte == b':')
            && colon > 0
            && line[..colon].iter().all(|byte| byte.is_ascii_graphic())
        {
            return true;
        }
        // First non-blank line is neither a header nor a diff: not a patch.
        break;
    }
    false
}

/// Split an mbox into individual messages and parse each into an [`AmPatch`].
///
/// Messages are delimited by lines beginning with `From ` (the mbox "From_"
/// separator that `git format-patch` emits as `From <sha> Mon Sep 17 …`). A
/// buffer with no separator at all is treated as a single message, matching
/// git's lenient behaviour for a lone patch. Whitespace-only input yields no
/// messages (the caller treats that as a no-op). A message that turns out to
/// carry no diff is still returned so the series driver can report the exact
/// "Patch is empty." behaviour git uses (including its hint block).
fn parse_mbox(input: &[u8]) -> Result<Vec<AmPatch>> {
    if input.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(Vec::new());
    }
    let lines = split_keep_newline(input);
    // Identify message-start indices (mbox "From " separators).
    let mut starts = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with(b"From ") {
            starts.push(idx);
        }
    }
    if starts.is_empty() {
        // No separator: the whole buffer is one message.
        return Ok(vec![parse_message(&lines)?]);
    }
    let mut patches = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        // Skip the leading "From " separator line itself.
        let body = &lines[start + 1..end];
        patches.push(parse_message(body)?);
    }
    Ok(patches)
}

/// Parse a single message (headers + blank line + body + diff).
fn parse_message(lines: &[Vec<u8>]) -> Result<AmPatch> {
    let mut author_name = String::new();
    let mut author_email = String::new();
    let mut author_date = None;
    let mut author_date_raw = None;
    let mut subject = String::new();

    // Phase 1: RFC822-style headers, ending at the first blank line. Continuation
    // lines (leading whitespace) extend the previous header value.
    let mut idx = 0;
    let mut last_header: Option<String> = None;
    let mut header_values: Vec<(String, String)> = Vec::new();
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        if line.is_empty() {
            idx += 1;
            break;
        }
        if (line[0] == b' ' || line[0] == b'\t') && last_header.is_some() {
            if let Some((_, value)) = header_values.last_mut() {
                value.push(' ');
                value.push_str(String::from_utf8_lossy(line).trim());
            }
            idx += 1;
            continue;
        }
        if let Some(colon) = line.iter().position(|byte| *byte == b':') {
            let name = String::from_utf8_lossy(&line[..colon])
                .trim()
                .to_lowercase();
            let value = String::from_utf8_lossy(&line[colon + 1..])
                .trim()
                .to_string();
            last_header = Some(name.clone());
            header_values.push((name, value));
        } else {
            // Not a header line — treat the rest as body (lenient).
            break;
        }
        idx += 1;
    }
    for (name, value) in &header_values {
        match name.as_str() {
            "from" => {
                let (name, email) = parse_from_header(value);
                author_name = name;
                author_email = email;
            }
            "date" => {
                author_date_raw = Some(value.clone());
                author_date = parse_rfc2822_date(value);
            }
            "subject" => subject = clean_subject(value),
            _ => {}
        }
    }

    // Phase 2: the rest of the message is one of three regions, in order:
    //   1. the commit body — until a standalone `---` separator or the diff;
    //   2. an optional diffstat — between the `---` separator and the diff,
    //      which `git format-patch` emits and `git am` discards;
    //   3. the diff itself — from the first `diff --git`/`Index:` line onward,
    //      ending at the `-- ` signature footer format-patch appends.
    #[derive(PartialEq)]
    enum Region {
        Body,
        Diffstat,
        Diff,
    }
    let mut body_lines: Vec<&[u8]> = Vec::new();
    let mut diff = Vec::new();
    let mut region = Region::Body;
    while idx < lines.len() {
        let raw = &lines[idx];
        let line = trim_trailing_newline(raw);
        match region {
            Region::Body => {
                if is_diff_start(line) {
                    region = Region::Diff;
                    diff.extend_from_slice(raw);
                } else if line == b"---" {
                    // End of the commit message; a diffstat (or the diff) follows.
                    region = Region::Diffstat;
                } else {
                    body_lines.push(raw);
                }
            }
            Region::Diffstat => {
                // Skip diffstat lines until the patch proper begins.
                if is_diff_start(line) {
                    region = Region::Diff;
                    diff.extend_from_slice(raw);
                }
            }
            Region::Diff => {
                if line == b"-- " {
                    break;
                }
                diff.extend_from_slice(raw);
            }
        }
        idx += 1;
    }

    let message = build_commit_message(&subject, &body_lines);

    Ok(AmPatch {
        author_name,
        author_email,
        author_date,
        author_date_raw,
        subject,
        message,
        diff,
    })
}

/// Parse a `From:` value of the form `Name <email>` (or a bare address).
fn parse_from_header(value: &str) -> (String, String) {
    if let Some(open) = value.rfind('<')
        && let Some(close) = value[open..].find('>')
    {
        let email = value[open + 1..open + close].trim().to_string();
        let name = decode_mime_word(value[..open].trim())
            .trim_matches('"')
            .to_string();
        return (name, email);
    }
    // Bare address: use it for both, matching git's fallback for name.
    let addr = value.trim().to_string();
    (addr.clone(), addr)
}

/// Strip a leading `[PATCH …]`/`[PATCH]` bracket prefix and surrounding space
/// from a subject, and decode a possible MIME encoded-word.
fn clean_subject(value: &str) -> String {
    let decoded = decode_mime_word(value);
    let mut subject = decoded.trim();
    // Remove one or more leading `[...]` brackets (e.g. `[PATCH 1/3]`, `[RFC]`).
    loop {
        let trimmed = subject.trim_start();
        if let Some(rest) = trimmed.strip_prefix('[')
            && let Some(close) = rest.find(']')
        {
            subject = rest[close + 1..].trim_start();
            continue;
        }
        break;
    }
    subject.trim().to_string()
}

/// Best-effort decode of a single RFC 2047 encoded-word for UTF-8/Q or B
/// encodings. Anything we cannot decode is returned unchanged.
fn decode_mime_word(value: &str) -> String {
    let trimmed = value.trim();
    let Some(rest) = trimmed.strip_prefix("=?") else {
        return value.to_string();
    };
    let Some(end) = rest.rfind("?=") else {
        return value.to_string();
    };
    let inner = &rest[..end];
    let parts: Vec<&str> = inner.splitn(3, '?').collect();
    if parts.len() != 3 {
        return value.to_string();
    }
    let charset = parts[0].to_ascii_lowercase();
    if charset != "utf-8" && charset != "us-ascii" {
        return value.to_string();
    }
    let decoded = match parts[1].to_ascii_uppercase().as_str() {
        "Q" => decode_quoted_printable_word(parts[2]),
        "B" => decode_base64(parts[2]),
        _ => return value.to_string(),
    };
    match decoded {
        Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        None => value.to_string(),
    }
}

fn decode_quoted_printable_word(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'_' => {
                out.push(b' ');
                idx += 1;
            }
            b'=' if idx + 2 < bytes.len() => {
                let hi = (bytes[idx + 1] as char).to_digit(16)?;
                let lo = (bytes[idx + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                idx += 3;
            }
            other => {
                out.push(other);
                idx += 1;
            }
        }
    }
    Some(out)
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input.bytes().filter(|byte| *byte != b'=').collect();
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in cleaned {
        let value = value(byte)?;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// Whether `line` begins a unified diff (a git or plain patch).
fn is_diff_start(line: &[u8]) -> bool {
    line.starts_with(b"diff --git ")
        || line.starts_with(b"--- ")
        || line.starts_with(b"diff --cc ")
        || line.starts_with(b"Index: ")
}

/// Build the full commit message: subject, blank line, then trimmed body.
///
/// Mirrors git's `cleanup`: the subject is the first line, followed by a blank
/// line and the body with leading/trailing blank lines removed. The result is
/// newline-terminated. An empty body yields just `subject\n`.
fn build_commit_message(subject: &str, body_lines: &[&[u8]]) -> Vec<u8> {
    // Drop leading and trailing blank lines from the body.
    let mut start = 0;
    while start < body_lines.len() && trim_trailing_newline(body_lines[start]).is_empty() {
        start += 1;
    }
    let mut end = body_lines.len();
    while end > start && trim_trailing_newline(body_lines[end - 1]).is_empty() {
        end -= 1;
    }
    let mut message = Vec::new();
    message.extend_from_slice(subject.as_bytes());
    message.push(b'\n');
    if end > start {
        message.push(b'\n');
        for line in &body_lines[start..end] {
            let trimmed = trim_trailing_newline(line);
            message.extend_from_slice(trimmed);
            message.push(b'\n');
        }
    }
    message
}

// ===========================================================================
// RFC 2822 date parsing → raw git timestamp
// ===========================================================================

/// Parse an RFC 2822 `Date:` value (e.g. `Sun, 27 Sep 2026 11:06:40 +0200`)
/// into git's raw `"<seconds> <±HHMM>"` form. Returns `None` if the value is not
/// in the expected shape, so callers can fall back to the environment date.
fn parse_rfc2822_date(value: &str) -> Option<String> {
    let mut tokens: Vec<&str> = value.split_whitespace().collect();
    // Optional leading weekday with trailing comma: "Sun," or "Sun".
    if let Some(first) = tokens.first() {
        let stripped = first.trim_end_matches(',');
        if WEEKDAYS.contains(&stripped) {
            tokens.remove(0);
        }
    }
    if tokens.len() < 5 {
        return None;
    }
    let day: u32 = tokens[0].parse().ok()?;
    let month = month_index(tokens[1])?;
    let year: i64 = tokens[2].parse().ok()?;
    let (hour, minute, second) = parse_clock(tokens[3])?;
    let timezone = parse_timezone(tokens[4])?;

    let days = days_from_civil(year, month, day as i64);
    let local_seconds = days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    let seconds = local_seconds - timezone.1;
    Some(format!("{seconds} {}", timezone.0))
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

fn month_index(token: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| month.eq_ignore_ascii_case(token))
        .map(|index| index as u32 + 1)
}

fn parse_clock(token: &str) -> Option<(u32, u32, u32)> {
    let mut parts = token.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = match parts.next() {
        Some(value) => value.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some((hour, minute, second))
}

/// Parse a timezone token (`+0200`, `-0500`, or a named zone) into its
/// canonical `±HHMM` string plus offset in seconds east of UTC.
fn parse_timezone(token: &str) -> Option<(String, i64)> {
    let bytes = token.as_bytes();
    if bytes.len() == 5
        && matches!(bytes[0], b'+' | b'-')
        && bytes[1..].iter().all(u8::is_ascii_digit)
    {
        let sign = if bytes[0] == b'+' { 1 } else { -1 };
        let hours: i64 = token[1..3].parse().ok()?;
        let minutes: i64 = token[3..5].parse().ok()?;
        let offset = sign * (hours * 3600 + minutes * 60);
        return Some((token.to_string(), offset));
    }
    // A handful of named zones from old mail (mostly UTC-equivalents).
    let offset = match token {
        "UT" | "GMT" | "UTC" | "Z" => 0,
        "EST" => -5 * 3600,
        "EDT" => -4 * 3600,
        "CST" => -6 * 3600,
        "CDT" => -5 * 3600,
        "MST" => -7 * 3600,
        "MDT" => -6 * 3600,
        "PST" => -8 * 3600,
        "PDT" => -7 * 3600,
        _ => return None,
    };
    Some((format_offset(offset), offset))
}

fn format_offset(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.abs();
    format!(
        "{sign}{:02}{:02}",
        magnitude / 3600,
        (magnitude % 3600) / 60
    )
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's algorithm).
/// Valid for the full proleptic Gregorian range; matches git's date arithmetic.
fn days_from_civil(year: i64, month: u32, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ===========================================================================
// State directory (.git/rebase-apply/)
// ===========================================================================

/// Create `.git/rebase-apply/` and populate it with the per-series control
/// files and one numbered file (`0001`, `0002`, …) per patch.
fn write_am_state_dir(
    state_dir: &Path,
    patches: &[AmPatch],
    options: &AmOptions,
    head_oid: &ObjectId,
) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    fs::write(state_dir.join("next"), b"1\n")?;
    fs::write(state_dir.join("last"), format!("{}\n", patches.len()))?;
    fs::write(state_dir.join("quiet"), bool_flag(options.quiet))?;
    fs::write(state_dir.join("sign"), bool_flag(options.signoff))?;
    fs::write(state_dir.join("threeway"), bool_flag(options.three_way))?;
    fs::write(state_dir.join("keep"), bool_flag(options.keep_non_patch))?;
    fs::write(state_dir.join("utf8"), b"t\n")?;
    fs::write(state_dir.join("applying"), b"")?;
    fs::write(state_dir.join("apply-opt"), b"")?;
    // abort-safety records the HEAD we started from so --abort can verify the
    // worktree has not been moved out from under us.
    fs::write(state_dir.join("abort-safety"), format!("{head_oid}\n"))?;
    for (index, patch) in patches.iter().enumerate() {
        let name = format!("{:04}", index + 1);
        fs::write(state_dir.join(name), encode_patch_file(patch))?;
    }
    Ok(())
}

fn bool_flag(value: bool) -> &'static [u8] {
    if value { b"t\n" } else { b"f\n" }
}

/// Reconstruct the numbered mbox-ish file for one patch (headers + body + diff),
/// matching the shape git stores so a human or `--show-current-patch` can read it.
fn encode_patch_file(patch: &AmPatch) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"From: ");
    out.extend_from_slice(patch.author_name.as_bytes());
    out.extend_from_slice(b" <");
    out.extend_from_slice(patch.author_email.as_bytes());
    out.extend_from_slice(b">\n");
    if let Some(date) = &patch.author_date_raw {
        out.extend_from_slice(b"Date: ");
        out.extend_from_slice(date.as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"Subject: [PATCH] ");
    out.extend_from_slice(patch.subject.as_bytes());
    out.extend_from_slice(b"\n\n");
    // Body (message minus the subject's first line).
    let body = commit_message_body_after_subject(&patch.message);
    out.extend_from_slice(&body);
    out.extend_from_slice(b"---\n\n");
    out.extend_from_slice(&patch.diff);
    out
}

/// Return the commit body (everything after the subject line and its trailing
/// blank line). Empty when the message is subject-only.
fn commit_message_body_after_subject(message: &[u8]) -> Vec<u8> {
    let Some(first_lf) = message.iter().position(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let mut start = first_lf + 1;
    if message.get(start) == Some(&b'\n') {
        start += 1;
    }
    message[start..].to_vec()
}

/// Write the per-patch control files git consults while a patch is current:
/// `author-script`, `info`, `final-commit`, `msg`, and `patch`.
fn write_current_patch_state(state_dir: &Path, patch: &AmPatch) -> Result<()> {
    let author_date = patch
        .author_date_raw
        .clone()
        .unwrap_or_else(default_author_date);
    let author_script = format!(
        "GIT_AUTHOR_NAME={}\nGIT_AUTHOR_EMAIL={}\nGIT_AUTHOR_DATE={}\n",
        shell_quote(&patch.author_name),
        shell_quote(&patch.author_email),
        shell_quote(&author_date),
    );
    fs::write(state_dir.join("author-script"), author_script)?;

    let info = format!(
        "Author: {}\nEmail: {}\nSubject: {}\nDate: {}\n\n",
        patch.author_name, patch.author_email, patch.subject, author_date,
    );
    fs::write(state_dir.join("info"), info)?;

    fs::write(state_dir.join("final-commit"), &patch.message)?;
    fs::write(state_dir.join("msg"), &patch.message)?;
    fs::write(state_dir.join("patch"), &patch.diff)?;
    Ok(())
}

fn default_author_date() -> String {
    env::var("GIT_AUTHOR_DATE").unwrap_or_else(|_| "@0 +0000".into())
}

/// Single-quote a value for the POSIX-sh `author-script`, escaping embedded
/// quotes the way git does (`'\''`).
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Read the numbered patch file `<n>` back into an [`AmPatch`] when resuming.
fn read_patch_file(state_dir: &Path, number: usize) -> Result<AmPatch> {
    let path = state_dir.join(format!("{number:04}"));
    let content = fs::read(&path)?;
    let lines = split_keep_newline(&content);
    parse_message(&lines)
}

fn read_state_usize(state_dir: &Path, name: &str) -> Result<usize> {
    let content = fs::read_to_string(state_dir.join(name))?;
    content
        .trim()
        .parse::<usize>()
        .map_err(|_| GitError::InvalidFormat(format!("invalid rebase-apply/{name}")))
}

fn read_state_bool(state_dir: &Path, name: &str) -> bool {
    fs::read_to_string(state_dir.join(name))
        .map(|content| content.trim() == "t")
        .unwrap_or(false)
}

// ===========================================================================
// Series driver
// ===========================================================================

/// Apply patches `start..=last` from the state directory, committing each.
///
/// On a clean apply this advances HEAD per patch and, after the final patch,
/// removes the state directory. On a conflict it leaves the state in place,
/// prints git's hint block, and exits 128 so the user can resolve and
/// `--continue` / `--skip` / `--abort`.
fn run_am_series(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
    start: usize,
) -> Result<()> {
    let last = read_state_usize(state_dir, "last")?;
    let quiet = read_state_bool(state_dir, "quiet");
    let signoff = read_state_bool(state_dir, "sign");
    let three_way = read_state_bool(state_dir, "threeway");
    let keep_non_patch = read_state_bool(state_dir, "keep");

    let mut number = start;
    while number <= last {
        fs::write(state_dir.join("next"), format!("{number}\n"))?;
        let patch = read_patch_file(state_dir, number)?;
        write_current_patch_state(state_dir, &patch)?;

        // A message that carried no diff stops the series with git's empty-patch
        // report (unless --keep/--keep-non-patch was requested).
        if patch.diff.is_empty() && !keep_non_patch {
            am_print_empty_patch_hints();
            println!("Patch is empty.");
            return Err(GitError::Exit(128));
        }

        if !quiet {
            println!("Applying: {}", patch.subject);
        }

        match apply_one_patch(
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            &patch,
            signoff,
            three_way,
        )? {
            ApplyResult::Committed => number += 1,
            ApplyResult::Conflict => {
                am_print_conflict_hints();
                println!("Patch failed at {number:04} {}", patch.subject);
                return Err(GitError::Exit(128));
            }
        }
    }

    finish_am(state_dir)
}

/// Outcome of attempting to apply (and commit) a single patch.
enum ApplyResult {
    Committed,
    Conflict,
}

/// Apply one patch's diff to the worktree+index and create the commit.
///
/// First tries straight application (the same engine `git apply` uses). If that
/// fails and `-3` was requested, falls back to a 3-way merge against the index's
/// recorded blobs. A clean result is committed and HEAD advanced; an unresolved
/// 3-way leaves conflict markers in the worktree and a conflicted index.
fn apply_one_patch(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    patch: &AmPatch,
    signoff: bool,
    three_way: bool,
) -> Result<ApplyResult> {
    let file_patches = sley_diff_merge::parse_unified_patch(&patch.diff)?;

    match try_straight_apply(worktree_root, &file_patches)? {
        Some(actions) => {
            apply_actions(worktree_root, &actions)?;
            stage_and_commit(
                git_dir,
                common_git_dir,
                worktree_root,
                format,
                patch,
                &actions,
                signoff,
            )?;
            Ok(ApplyResult::Committed)
        }
        None => {
            if three_way {
                println!("Using index info to reconstruct a base tree...");
                return apply_three_way(
                    git_dir,
                    common_git_dir,
                    worktree_root,
                    format,
                    patch,
                    &file_patches,
                    signoff,
                );
            }
            for file in &file_patches {
                let name = file
                    .new_path
                    .as_deref()
                    .or(file.old_path.as_deref())
                    .unwrap_or(b"");
                eprintln!("error: patch failed: {}:1", String::from_utf8_lossy(name));
                eprintln!(
                    "error: {}: patch does not apply",
                    String::from_utf8_lossy(name)
                );
            }
            Ok(ApplyResult::Conflict)
        }
    }
}

/// A single materialisation step computed from a patch (write or remove a file).
enum ApplyFileAction {
    Write {
        path: Vec<u8>,
        mode: u32,
        content: Vec<u8>,
    },
    Remove {
        path: Vec<u8>,
    },
}

/// Compute the file actions for every hunk against the current worktree, or
/// `None` if any hunk fails to apply (so the whole patch is atomic, like git).
fn try_straight_apply(
    worktree_root: &Path,
    file_patches: &[sley_diff_merge::FilePatch],
) -> Result<Option<Vec<ApplyFileAction>>> {
    let mut actions = Vec::new();
    for patch in file_patches {
        let base = if patch.is_new {
            Vec::new()
        } else if let Some(old) = patch.old_path.as_deref().or(patch.new_path.as_deref()) {
            let rel = std::str::from_utf8(old)
                .map_err(|_| GitError::InvalidFormat("non-utf8 patch path".into()))?;
            fs::read(worktree_root.join(rel)).unwrap_or_default()
        } else {
            Vec::new()
        };
        let content = match sley_diff_merge::apply_file_patch(&base, patch) {
            sley_diff_merge::ApplyOutcome::Applied(content) => content,
            sley_diff_merge::ApplyOutcome::Rejected => return Ok(None),
        };
        if patch.is_delete {
            if let Some(old) = &patch.old_path {
                actions.push(ApplyFileAction::Remove { path: old.clone() });
            }
        } else {
            let mode = patch.new_mode.or(patch.old_mode).unwrap_or(0o100644);
            let Some(target) = patch.new_path.clone().or_else(|| patch.old_path.clone()) else {
                return Err(GitError::InvalidFormat("patch missing target path".into()));
            };
            actions.push(ApplyFileAction::Write {
                path: target,
                mode,
                content,
            });
            if patch.is_rename
                && let Some(old) = &patch.old_path
            {
                actions.push(ApplyFileAction::Remove { path: old.clone() });
            }
        }
    }
    Ok(Some(actions))
}

fn apply_actions(worktree_root: &Path, actions: &[ApplyFileAction]) -> Result<()> {
    for action in actions {
        match action {
            ApplyFileAction::Write {
                path,
                mode,
                content,
            } => merge_write_worktree_file(worktree_root, path, content, *mode)?,
            ApplyFileAction::Remove { path } => merge_remove_worktree_file(worktree_root, path)?,
        }
    }
    Ok(())
}

/// Stage the files this patch touched into the index and create the commit,
/// advancing HEAD (or the branch HEAD points at) with an `am` reflog entry.
fn stage_and_commit(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    patch: &AmPatch,
    actions: &[ApplyFileAction],
    signoff: bool,
) -> Result<()> {
    let mut db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut index = read_repository_index(git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });

    for action in actions {
        match action {
            ApplyFileAction::Write {
                path,
                mode,
                content,
            } => {
                let oid = db.write_object(EncodedObject::new(ObjectType::Blob, content.clone()))?;
                upsert_index_entry(&mut index, path, *mode, oid);
            }
            ApplyFileAction::Remove { path } => {
                index.entries.retain(|entry| &entry.path != path);
            }
        }
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        index.write(format)?,
    )?;

    create_am_commit(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        patch,
        signoff,
    )
}

/// Insert or replace the stage-0 index entry for `path`.
fn upsert_index_entry(index: &mut Index, path: &[u8], mode: u32, oid: ObjectId) {
    let entry = merge_index_entry(path, mode, oid, 0);
    if let Some(existing) = index
        .entries
        .iter_mut()
        .find(|candidate| candidate.path == path)
    {
        *existing = entry;
    } else {
        index.entries.push(entry);
    }
}

/// Build the commit from the current index tree, using the patch's author
/// identity/date and the environment committer, then advance HEAD.
fn create_am_commit(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    patch: &AmPatch,
    signoff: bool,
) -> Result<()> {
    let refs = FileRefStore::new(git_dir, format);
    let head_oid = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("am: HEAD disappeared mid-series".into()))?;
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;

    let author = am_author_identity(patch)?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let message = if signoff {
        commit_message_with_signoff(patch.message.clone(), &commit_signoff_from_env()?)
    } else {
        patch.message.clone()
    };

    let mut db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let new_oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![head_oid],
            author,
            committer: committer.clone(),
            message,
        },
    )?;

    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: Some(RefTarget::Direct(head_oid)),
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid: head_oid,
            new_oid,
            committer,
            message: format!("am: {}", patch.subject).into_bytes(),
        }),
    });
    tx.commit()?;
    sley_worktree::reset_index_and_worktree_to_commit(worktree_root, git_dir, format, &new_oid)?;
    Ok(())
}

/// Build the author identity bytes from the patch headers, falling back to the
/// environment author date when the email had no parsable `Date:`.
fn am_author_identity(patch: &AmPatch) -> Result<Vec<u8>> {
    let date = patch
        .author_date
        .clone()
        .unwrap_or_else(|| env::var("GIT_AUTHOR_DATE").unwrap_or_else(|_| "@0 +0000".into()));
    sley_sequencer::format_commit_identity(&patch.author_name, &patch.author_email, &date)
}

/// Best-effort 3-way application: reconstruct the pre-image from the index's
/// blobs, apply the patch to that to form "theirs", and 3-way merge against the
/// worktree state ("ours"). Reuses the shared tree-merge engine.
fn apply_three_way(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    patch: &AmPatch,
    file_patches: &[sley_diff_merge::FilePatch],
    signoff: bool,
) -> Result<ApplyResult> {
    let refs = FileRefStore::new(git_dir, format);
    let head_oid = head_commit_oid(&refs)?
        .ok_or_else(|| GitError::Command("am: HEAD disappeared mid-series".into()))?;
    let mut db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let ours_map = stash_tree_entry_map(&db, format, &head_tree)?;

    // The merge base for each file is the patch's *pre-image* blob, named by the
    // old side of its `index <old>..<new>` line. Looking those blobs up in the
    // object store (the same thing git does for `am -3`) reconstructs a base tree
    // that may differ from HEAD, which is exactly what lets a 3-way merge succeed
    // when straight application failed.
    let index_oids = parse_patch_index_oids(&patch.diff);

    let mut base_map = ours_map.clone();
    let mut theirs_map = ours_map.clone();
    for file in file_patches {
        let path = file
            .new_path
            .clone()
            .or_else(|| file.old_path.clone())
            .ok_or_else(|| GitError::InvalidFormat("patch missing target path".into()))?;
        let old_path = file.old_path.clone().unwrap_or_else(|| path.clone());

        let base_bytes = if file.is_new {
            Vec::new()
        } else if let Some(bytes) =
            lookup_patch_base_blob(&db, &index_oids, &path, &old_path, &ours_map)?
        {
            bytes
        } else {
            // We cannot reconstruct a base for this path: fail the 3-way.
            eprintln!("error: repository lacks the necessary blob to fall back on 3-way merge.");
            eprintln!("error: Failed to merge in the changes.");
            return Ok(ApplyResult::Conflict);
        };

        // Default modes to the current HEAD entry's mode (or 644) when the patch
        // carries no explicit mode header, so an unchanged mode never looks like
        // a mode conflict to the tree merge.
        let inherited_mode = ours_map
            .get(&old_path)
            .or_else(|| ours_map.get(&path))
            .map(|(mode, _)| *mode)
            .unwrap_or(0o100644);
        match sley_diff_merge::apply_file_patch(&base_bytes, file) {
            sley_diff_merge::ApplyOutcome::Applied(post) => {
                let mode = file.new_mode.or(file.old_mode).unwrap_or(inherited_mode);
                let base_mode = file.old_mode.unwrap_or(inherited_mode);
                if file.is_new {
                    base_map.remove(&path);
                } else {
                    let base_oid =
                        db.write_object(EncodedObject::new(ObjectType::Blob, base_bytes))?;
                    base_map.insert(old_path.clone(), (base_mode, base_oid));
                }
                if file.is_delete {
                    theirs_map.remove(&path);
                } else {
                    let post_oid = db.write_object(EncodedObject::new(ObjectType::Blob, post))?;
                    theirs_map.insert(path.clone(), (mode, post_oid));
                    if file.is_rename {
                        theirs_map.remove(&old_path);
                    }
                }
            }
            sley_diff_merge::ApplyOutcome::Rejected => {
                eprintln!("error: Failed to merge in the changes.");
                return Ok(ApplyResult::Conflict);
            }
        }
    }

    // Report the paths that differ between the reconstructed base and HEAD, the
    // way git's "reconstruct a base tree" step does (`<status>\t<path>`).
    print_three_way_base_status(&base_map, &ours_map);

    println!("Falling back to patching base and 3-way merge...");
    let (results, conflicts) = three_way_merge_trees(
        &mut db,
        &base_map,
        &ours_map,
        &theirs_map,
        "HEAD",
        &patch.subject,
    )?;

    // git prints "Auto-merging <path>" for every file changed on both sides.
    for path in three_way_auto_merged_paths(&base_map, &ours_map, &theirs_map) {
        println!("Auto-merging {}", String::from_utf8_lossy(&path));
    }

    write_merge_index_and_worktree(git_dir, worktree_root, format, &db, &ours_map, &results)?;

    if conflicts.is_empty() {
        create_am_commit(
            git_dir,
            common_git_dir,
            worktree_root,
            format,
            patch,
            signoff,
        )?;
        Ok(ApplyResult::Committed)
    } else {
        for path in &conflicts {
            println!(
                "CONFLICT (content): Merge conflict in {}",
                String::from_utf8_lossy(path)
            );
        }
        eprintln!("error: Failed to merge in the changes.");
        Ok(ApplyResult::Conflict)
    }
}

/// Print the `<status>\t<path>` lines git emits while reconstructing the base
/// tree for a 3-way merge: `A` added, `D` deleted, `M` modified relative to the
/// reconstructed base.
fn print_three_way_base_status(base_map: &MergeTreeMap, ours_map: &MergeTreeMap) {
    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(base_map.keys());
    paths.extend(ours_map.keys());
    for path in paths {
        let status = match (base_map.get(path), ours_map.get(path)) {
            (Some(base), Some(ours)) if base != ours => Some('M'),
            (None, Some(_)) => Some('A'),
            (Some(_), None) => Some('D'),
            _ => None,
        };
        if let Some(status) = status {
            println!("{status}\t{}", String::from_utf8_lossy(path));
        }
    }
}

/// Paths changed on both sides of the merge (base→ours and base→theirs both
/// differ) — the files git announces with "Auto-merging".
fn three_way_auto_merged_paths(
    base_map: &MergeTreeMap,
    ours_map: &MergeTreeMap,
    theirs_map: &MergeTreeMap,
) -> Vec<Vec<u8>> {
    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(base_map.keys());
    paths.extend(ours_map.keys());
    paths.extend(theirs_map.keys());
    paths
        .into_iter()
        .filter(|path| {
            base_map.get(*path) != ours_map.get(*path)
                && base_map.get(*path) != theirs_map.get(*path)
        })
        .cloned()
        .collect()
}

/// Map each touched path to the abbreviated old-blob OID from its
/// `index <old>..<new>` header line, keyed by the `b/` (new) path. Used by the
/// 3-way fallback to find the patch's pre-image blob in the object store.
fn parse_patch_index_oids(diff: &[u8]) -> BTreeMap<Vec<u8>, String> {
    let mut map = BTreeMap::new();
    let mut current_path: Option<Vec<u8>> = None;
    for line in split_keep_newline(diff) {
        let line = trim_trailing_newline(&line);
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            current_path = parse_diff_git_new_path(rest);
        } else if let Some(rest) = line.strip_prefix(b"index ") {
            let text = String::from_utf8_lossy(rest);
            if let Some(path) = current_path.clone()
                && let Some((old, _)) = text.split_once("..")
                && !old.trim().is_empty()
                && old.trim().bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                map.insert(path, old.trim().to_string());
            }
        }
    }
    map
}

/// Extract the `b/<path>` component from a `diff --git a/<path> b/<path>` line.
fn parse_diff_git_new_path(rest: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(rest);
    // The new side begins at the last " b/" occurrence (paths may contain spaces
    // but format-patch emits unquoted `a/… b/…` for ordinary names).
    let marker = text.rfind(" b/")?;
    let path = &text[marker + 3..];
    Some(path.as_bytes().to_vec())
}

/// Read the pre-image blob for `path`: resolve the patch's recorded old OID in
/// the object store, falling back to HEAD's blob for the path. Returns `None`
/// when neither source can supply the base content.
fn lookup_patch_base_blob(
    db: &FileObjectDatabase,
    index_oids: &BTreeMap<Vec<u8>, String>,
    path: &[u8],
    old_path: &[u8],
    ours_map: &MergeTreeMap,
) -> Result<Option<Vec<u8>>> {
    if let Some(prefix) = index_oids.get(path)
        && let Ok(ObjectPrefixResolution::Unique(oid)) = db.resolve_prefix(prefix)
    {
        let object = db.read_object(&oid)?;
        if object.object_type == ObjectType::Blob {
            return Ok(Some(object.body.clone()));
        }
    }
    if let Some((_, oid)) = ours_map.get(old_path).or_else(|| ours_map.get(path)) {
        return Ok(Some(merge_read_blob(db, oid)?));
    }
    Ok(None)
}

/// Materialise a 3-way merge result into the index (with conflict stages) and
/// the worktree (with conflict markers for unresolved paths).
fn write_merge_index_and_worktree(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    ours_map: &MergeTreeMap,
    results: &BTreeMap<Vec<u8>, MergePathResult>,
) -> Result<()> {
    let mut entries = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
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
            .then_with(|| (left.flags >> 12).cmp(&(right.flags >> 12)))
    });
    let index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        index.write(format)?,
    )?;

    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = merge_read_blob(db, oid)?;
                    merge_write_worktree_file(worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => merge_remove_worktree_file(worktree_root, path)?,
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    merge_write_worktree_file(worktree_root, path, content, *mode)?
                }
                None => merge_remove_worktree_file(worktree_root, path)?,
            },
        }
    }
    Ok(())
}

fn am_print_conflict_hints() {
    eprintln!("hint: Use 'git am --show-current-patch=diff' to see the failed patch");
    eprintln!("hint: When you have resolved this problem, run \"git am --continue\".");
    eprintln!("hint: If you prefer to skip this patch, run \"git am --skip\" instead.");
    eprintln!("hint: To restore the original branch and stop patching, run \"git am --abort\".");
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

fn am_print_empty_patch_hints() {
    eprintln!("hint: When you have resolved this problem, run \"git am --continue\".");
    eprintln!("hint: If you prefer to skip this patch, run \"git am --skip\" instead.");
    eprintln!("hint: To record the empty patch as an empty commit, run \"git am --allow-empty\".");
    eprintln!("hint: To restore the original branch and stop patching, run \"git am --abort\".");
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

/// Render the state directory path the way git reports it in the
/// "previous rebase directory … still exists" error: relative to the worktree
/// root when possible (`.git/rebase-apply`), else the absolute path.
fn display_state_dir(worktree_root: &Path, state_dir: &Path) -> String {
    match state_dir.strip_prefix(worktree_root) {
        Ok(relative) => relative.display().to_string(),
        Err(_) => state_dir.display().to_string(),
    }
}

/// Remove the state directory after the last patch lands successfully.
fn finish_am(state_dir: &Path) -> Result<()> {
    if state_dir.exists() {
        fs::remove_dir_all(state_dir)?;
    }
    Ok(())
}

// ===========================================================================
// Resume sub-operations
// ===========================================================================

fn am_require_in_progress(state_dir: &Path) -> Result<()> {
    if !state_dir.exists() {
        eprintln!("fatal: Resolve operation not in progress, we are not resuming.");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// `git am --abort`: restore the branch to where the series started and drop
/// the state directory.
fn am_abort(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    let safety = fs::read_to_string(state_dir.join("abort-safety")).unwrap_or_default();
    let safety = safety.trim();
    if !safety.is_empty()
        && let Ok(oid) = ObjectId::from_hex(format, safety)
    {
        let refs = FileRefStore::new(git_dir, format);
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let current = head_commit_oid(&refs)?;
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: Some(ReflogEntry {
                old_oid: current.unwrap_or(zero_oid(format)?),
                new_oid: oid,
                committer: commit_identity_from_env("COMMITTER")?,
                message: b"am --abort".to_vec(),
            }),
        });
        tx.commit()?;
        sley_worktree::reset_index_and_worktree_to_commit(worktree_root, git_dir, format, &oid)?;
    }
    finish_am(state_dir)
}

/// `git am --quit`: leave HEAD and the worktree as-is, only drop the state.
fn am_quit(state_dir: &Path) -> Result<()> {
    am_require_in_progress(state_dir)?;
    finish_am(state_dir)
}

/// `git am --skip`: discard the current patch's partial state, reset the
/// worktree/index to HEAD, and resume with the next patch.
fn am_skip(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    let head_oid = resolve_revision(git_dir, format, "HEAD")?;
    sley_worktree::reset_index_and_worktree_to_commit(worktree_root, git_dir, format, &head_oid)?;
    let next = read_state_usize(state_dir, "next")?;
    run_am_series(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        state_dir,
        next + 1,
    )
}

/// `git am --continue`/`--resolved`: commit the staged resolution of the current
/// patch using its preserved author/message, then resume with the next patch.
fn am_continue(
    git_dir: &Path,
    common_git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    state_dir: &Path,
) -> Result<()> {
    am_require_in_progress(state_dir)?;
    let signoff = read_state_bool(state_dir, "sign");
    let quiet = read_state_bool(state_dir, "quiet");
    let next = read_state_usize(state_dir, "next")?;
    let patch = read_patch_file(state_dir, next)?;

    if !quiet {
        println!("Applying: {}", patch.subject);
    }

    // Refuse if the index still has unmerged entries (unresolved conflicts).
    if let Some(index) = read_repository_index(git_dir, format)?
        && index
            .entries
            .iter()
            .any(|entry| (entry.flags >> 12) & 0x3 != 0)
    {
        am_print_conflict_hints();
        println!("Patch failed at {next:04} {}", patch.subject);
        return Err(GitError::Exit(128));
    }

    create_am_commit(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        &patch,
        signoff,
    )?;
    run_am_series(
        git_dir,
        common_git_dir,
        worktree_root,
        format,
        state_dir,
        next + 1,
    )
}

// ===========================================================================
// Small byte helpers
// ===========================================================================

/// Split a buffer into lines, each retaining its trailing `\n` (the final line
/// keeps whatever terminator it had, or none).
fn split_keep_newline(input: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(input[start..=idx].to_vec());
            start = idx + 1;
        }
    }
    if start < input.len() {
        lines.push(input[start..].to_vec());
    }
    lines
}

/// A line without its trailing `\r?\n`.
fn trim_trailing_newline(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}
