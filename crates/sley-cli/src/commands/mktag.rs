//! `git mktag` — build a tag object from a payload read on stdin, with strict
//! fsck validation.
//!
//! Real `git mktag` reads a complete tag-object payload from standard input,
//! runs Git's tag fsck over it, verifies the tagged object exists and has the
//! declared type, writes the payload to the object database *verbatim*, and
//! prints the resulting object id. This module reproduces that contract
//! byte-for-byte against the system `git`, faithfully mirroring git's
//! `fsck_tag_standalone` (fsck.c) — including its quirky reporting control flow.
//!
//!   * **Verbatim write.** The payload is hashed and stored exactly as received;
//!     `mktag` never reparses-and-reserializes it. An uppercase `object` SHA or a
//!     message with no trailing newline is preserved, so the object id matches
//!     git's. We build the object with [`EncodedObject::new`] from the raw bytes
//!     rather than going through [`Tag::write`].
//!   * **fsck.** The payload must have `object`/`type`/`tag`/`tagger` headers in
//!     that order, a valid tagged-object SHA, a known object type, a tag name
//!     that passes `check_refname_format`, and a well-formed tagger identity line
//!     (optionally followed by `gpgsig`/`gpgsig-sha256` headers). Each problem is
//!     reported as `<sev>: tag input does not pass fsck: <msgid>: <detail>` where
//!     `<sev>` is `error` (the default/strict, where every message is promoted to
//!     an error) or `warning` (`--no-strict`, for messages git classifies as
//!     warnings: `badTagName`, `missingTaggerEntry`, `extraHeaderEntry`).
//!   * **Reporting control flow (the subtle part).** git's `fsck_tag_standalone`
//!     keeps a single `ret` and, after each check, runs `ret = report(...)`. The
//!     *structural* checks (header NUL/termination, `object`/`type`/`tag` lines)
//!     `goto done` the moment `report` returns non-zero, so they short-circuit.
//!     But the tag-name and tagger checks `goto done` only on a non-zero `report`
//!     for `badTagName`/`missingTaggerEntry`, while a `fsck_ident` failure
//!     **falls through** to the `gpgsig`/extra-header checks, whose `report`
//!     overwrites `ret`. The command dies iff the *final* `ret` is non-zero. Two
//!     consequences this module reproduces exactly. First, in `--no-strict`, an
//!     identity error followed by a trailing warning (e.g. `extraHeaderEntry`) is
//!     masked: the warning's `report` returns 0, so the tag is written despite
//!     the printed `error:` line. Second, in strict mode
//!     `badTagName`/`missingTaggerEntry` abort immediately (later problems are
//!     not printed), but an identity error does not — it and a following
//!     `extraHeaderEntry` are both printed before the fatal line. On any fatal
//!     outcome a trailing
//!     `fatal: tag on stdin did not pass our strict fsck check` is printed and
//!     the command exits 128.
//!   * **Tagged-object checks.** After a passing fsck, the tagged object is read:
//!     a missing object reports `fatal: could not read tagged object '<oid>'`,
//!     and a type mismatch reports
//!     `fatal: object '<oid>' tagged as '<declared>', but is a '<actual>' type`.
//!     These are *not* wrapped in the fsck framing and use the **canonical**
//!     (lowercase) object id, even when the payload's `object` line used
//!     uppercase hex.
//!   * **CLI.** The sole option is `--[no-]strict`. `-h` and `--help-all` print
//!     the short usage to stdout (exit 129), matching git exactly. `--help` is
//!     also treated as a short-usage request here: real git's main dispatcher
//!     routes `git <cmd> --help` to the man page (exit 0), but this CLI has no
//!     man-page backend, so — like the sibling `commands::verify_commit` — we
//!     render the short usage instead. An unknown option/switch prints
//!     `error: unknown option/switch ...` plus usage to stderr (exit 129);
//!     `--strict=<v>` prints `error: option 'strict' takes no value` with no
//!     usage block (exit 129). Non-option operands are ignored — `mktag` reads
//!     only stdin.
//!
//! This module follows the glob-import + private-helper structure of the other
//! self-contained command modules (`commands::verify_commit`, `commands::tag`,
//! `commands::stash`).

// Glob the crate root for shared CLI rendering and diagnostics (EncodedObject,
// ObjectType, ObjectId, GitError, io, etc.); see commands::stash for the
// rationale behind the wildcard import.
use crate::*;

/// Entry point for `git mktag`.
pub(crate) fn cmd_mktag(cli_session: &session::CliSession, args: &[String]) -> Result<()> {
    let strict = match parse_mktag_args(args)? {
        MktagInvocation::Run { strict } => strict,
        MktagInvocation::Help => {
            print!("{MKTAG_USAGE}");
            io::stdout().flush()?;
            return Err(GitError::Exit(129));
        }
    };

    // Discover the repository up front. git prints its standard
    // "not a git repository" fatal (exit 128) before inspecting stdin's contents.
    let repo = match cli_session.open_repository() {
        Ok(repo) => repo,
        Err(GitError::NotFound(_)) => return mktag_not_a_repository(),
        Err(err) => return Err(err),
    };
    let format = repo.object_format();
    let config = read_repo_config(repo.git_dir())?;

    // Read the entire payload. mktag is binary-safe: the buffer is stored exactly
    // as received once it passes validation.
    let mut payload = Vec::new();
    io::stdin().read_to_end(&mut payload)?;

    // fsck the payload (prints any problems inline, like git). A fatal outcome
    // ends with the trailing fatal line and exit 128.
    let mut reporter = FsckReporter::from_repo(strict, &config);
    let parsed = fsck_tag(format, &payload, &mut reporter);
    if reporter.is_fatal() {
        eprintln!("{FSCK_FATAL_TEXT}");
        return Err(GitError::Exit(128));
    }
    // A non-fatal fsck guarantees the structural headers parsed; the explicit
    // guard keeps the contract clear without an unwrap.
    let Some(parsed) = parsed else {
        return Err(GitError::Exit(128));
    };

    // The tagged object must exist and match the declared type. These checks are
    // reported without the fsck framing and use the canonical object id.
    verify_tagged_object(cli_session.replace_objects(), &repo, &parsed)?;

    // Write the payload verbatim and print the resulting object id.
    let oid = repo.write_object(EncodedObject::new(ObjectType::Tag, payload))?;
    println!("{oid}");
    Ok(())
}

/// The outcome of argument parsing: a runnable invocation or a help request.
#[derive(Debug, PartialEq, Eq)]
enum MktagInvocation {
    Run { strict: bool },
    Help,
}

/// Parse `mktag` arguments. The grammar mirrors git's parse-options: a single
/// `--[no-]strict` toggle (default on), `--`/`-h`/`--help`/`--help-all`, and
/// exit-129 errors for unknown options/switches or a value on `--strict`.
/// Non-option operands are accepted and ignored (git reads only stdin).
fn parse_mktag_args(args: &[String]) -> Result<MktagInvocation> {
    let mut strict = true;
    // mktag never reads an option *value* (its sole option, --[no-]strict, is a
    // bare toggle), so a plain `for` over the arguments suffices; remaining
    // operands after `--` are simply ignored.
    for arg in args {
        match arg.as_str() {
            // `--` ends option processing; remaining tokens are (ignored) operands.
            "--" | "--end-of-options" => break,
            "-h" | "--help" | "--help-all" => return Ok(MktagInvocation::Help),
            "--strict" => strict = true,
            "--no-strict" => strict = false,
            value if value.starts_with("--strict=") => {
                return mktag_option_takes_no_value_error("strict");
            }
            value if value.starts_with("--no-strict=") => {
                return mktag_option_takes_no_value_error("no-strict");
            }
            value if value.starts_with("--") => {
                return mktag_unknown_option_error(value.trim_start_matches("--"));
            }
            value if value.starts_with('-') && value.len() > 1 => {
                // mktag has no short switches, so the first character after `-`
                // is always unknown, matching git's `error: unknown switch`.
                let switch = value.chars().nth(1).unwrap_or('-');
                return mktag_unknown_switch_error(switch);
            }
            // Any other operand is ignored: mktag's input is stdin only.
            _ => {}
        }
    }
    Ok(MktagInvocation::Run { strict })
}

/// Severity of an fsck message. Warning-severity messages are tolerated under
/// `--no-strict`; under strict every message is promoted to an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FsckSeverity {
    Ignore,
    Warn,
    Error,
}

/// Prints fsck problems with git's framing and tracks whether the command should
/// ultimately die. Mirrors git's `report()`/`ret` pair: [`report`] prints one
/// line and returns the value git's `report()` would (`true` = non-zero = would
/// trigger the fatal path), and [`is_fatal`] holds the *latest* such value, since
/// git's `ret` is overwritten by each successive check.
///
/// [`report`]: FsckReporter::report
/// [`is_fatal`]: FsckReporter::is_fatal
struct FsckReporter {
    strict: bool,
    extra_header_entry: FsckSeverity,
    /// The value of the most recent `report()` call, i.e. git's final `ret`.
    fatal: bool,
}

impl FsckReporter {
    fn new(strict: bool) -> Self {
        Self {
            strict,
            extra_header_entry: FsckSeverity::Warn,
            fatal: false,
        }
    }

    fn from_repo(strict: bool, config: &GitConfig) -> Self {
        let mut reporter = Self::new(strict);
        for (key, value) in config.fsck_entries() {
            if key.eq_ignore_ascii_case("extraHeaderEntry") {
                match value.trim().to_ascii_lowercase().as_str() {
                    "error" => reporter.extra_header_entry = FsckSeverity::Error,
                    "warn" | "warning" => reporter.extra_header_entry = FsckSeverity::Warn,
                    "ignore" => reporter.extra_header_entry = FsckSeverity::Ignore,
                    _ => {}
                }
            }
        }
        reporter
    }

    /// Report one fsck problem. Prints `error:`/`warning:` per the effective
    /// severity (strict promotes warnings to errors) and returns whether git's
    /// `report()` would be non-zero — which the caller uses to decide whether to
    /// `goto done`. Also records that value as the running fatal state.
    fn report(&mut self, severity: FsckSeverity, id: &str, detail: &str) -> bool {
        if severity == FsckSeverity::Ignore {
            self.fatal = false;
            return false;
        }
        let is_error = self.strict || severity == FsckSeverity::Error;
        let prefix = if is_error { "error" } else { "warning" };
        eprintln!("{prefix}: tag input does not pass fsck: {id}: {detail}");
        self.fatal = is_error;
        is_error
    }

    /// Whether the command should die (print the fatal line, exit 128) — i.e.
    /// git's final `ret` was non-zero.
    fn is_fatal(&self) -> bool {
        self.fatal
    }
}

/// The header fields the tagged-object check needs: the parsed (canonical) object
/// id and the declared `type`.
struct ParsedTag {
    object_id: ObjectId,
    declared_type: ObjectType,
}

/// fsck the tag `payload`, printing any problems through `reporter` in git's
/// order and control flow (see the module docs). Returns the parsed structural
/// headers when they were well-formed enough to identify the tagged object
/// (i.e. the `object` and `type` lines parsed), so the caller can run the
/// tagged-object check; returns `None` when a structural header failed.
///
/// This is a direct port of `fsck_tag_standalone` in git's fsck.c: each check
/// assigns the reporter's return to a running `ret`, the structural checks
/// short-circuit on a non-zero `ret`, and the command's fatality is the final
/// `ret` (tracked inside the reporter).
fn fsck_tag(
    format: ObjectFormat,
    payload: &[u8],
    reporter: &mut FsckReporter,
) -> Option<ParsedTag> {
    // (1) verify_headers: stop immediately on failure (git relies on this for the
    // memory-safety of the rest; for us it means no further parsing).
    if verify_headers(payload, reporter) {
        return None;
    }

    let mut cursor = LineCursor::new(payload);

    // object <sha>\n  (structural: goto done on failure)
    let Some(object_value) = cursor
        .next_header_line()
        .and_then(|line| strip_header_prefix(line, b"object "))
    else {
        reporter.report(
            FsckSeverity::Error,
            "missingObject",
            "invalid format - expected 'object' line",
        );
        return None;
    };
    let Some(object_id) = parse_oid_line(format, object_value) else {
        reporter.report(
            FsckSeverity::Error,
            "badObjectSha1",
            "invalid 'object' line format - bad sha1",
        );
        return None;
    };

    // type <type>\n  (structural)
    let Some(type_value) = cursor
        .next_header_line()
        .and_then(|line| strip_header_prefix(line, b"type "))
    else {
        reporter.report(
            FsckSeverity::Error,
            "missingTypeEntry",
            "invalid format - expected 'type' line",
        );
        return None;
    };
    let Some(declared_type) = parse_object_type(type_value) else {
        reporter.report(FsckSeverity::Error, "badType", "invalid 'type' value");
        return None;
    };

    // tag <name>\n  (structural for presence; the name check itself is soft)
    let Some(tag_value) = cursor
        .next_header_line()
        .and_then(|line| strip_header_prefix(line, b"tag "))
    else {
        reporter.report(
            FsckSeverity::Error,
            "missingTagEntry",
            "invalid format - expected 'tag' line",
        );
        return None;
    };

    // From here, git has identified the tagged object; record it so the caller
    // can run the tagged-object check even if a soft problem follows.
    let parsed = ParsedTag {
        object_id,
        declared_type,
    };

    // tag name validity: `if (report) goto done` — a non-zero report aborts.
    if !check_refname_format(tag_value) {
        let detail = format!("invalid 'tag' name: {}", String::from_utf8_lossy(tag_value));
        if reporter.report(FsckSeverity::Warn, "badTagName", &detail) {
            return Some(parsed);
        }
    }

    // tagger line. Two branches with different control flow, matching git:
    //   * absent: report missingTaggerEntry; `if (report) goto done`.
    //   * present: run fsck_ident, which reports at most one problem and does
    //     NOT goto done — execution falls through to the header checks below,
    //     whose report overwrites `ret`.
    match cursor
        .next_header_line()
        .and_then(|line| strip_header_prefix(line, b"tagger "))
    {
        Some(ident) => {
            fsck_ident(ident, reporter);
        }
        None => {
            if reporter.report(
                FsckSeverity::Warn,
                "missingTaggerEntry",
                "invalid format - expected 'tagger' line",
            ) {
                return Some(parsed);
            }
            // git did not consume a line for the absent tagger; rewind so the
            // header checks below inspect the same line.
            cursor.rewind_last();
        }
    }

    // Optional gpgsig / gpgsig-sha256 header (a signed tag), with folded
    // continuation lines. These are NOT extra headers.
    cursor.skip_gpgsig_header();

    // Any remaining non-blank header line is an extra entry: `if (report) goto
    // done`. (This report overwrites `ret`, which is what masks a preceding
    // identity error under --no-strict.)
    if cursor.has_extra_header() {
        reporter.report(
            reporter.extra_header_entry,
            "extraHeaderEntry",
            "invalid format - extra header(s) after 'tagger'",
        );
    }

    Some(parsed)
}

/// git's `verify_headers`: scan the header region for an embedded NUL
/// (`nulInHeader`, with the 0-based offset) and require it to end with a newline
/// (`unterminatedHeader`). The scan stops at the first `"\n\n"` (so a NUL in the
/// message body is allowed). Returns true when a problem was reported (the caller
/// must then stop parsing).
fn verify_headers(payload: &[u8], reporter: &mut FsckReporter) -> bool {
    // Mirror git's byte loop: a NUL anywhere before the header/message separator
    // is reported with its offset; reaching "\n\n" ends the header region cleanly.
    let mut idx = 0;
    while idx < payload.len() {
        match payload[idx] {
            0 => {
                reporter.report(
                    FsckSeverity::Error,
                    "nulInHeader",
                    &format!("unterminated header: NUL at offset {idx}"),
                );
                return true;
            }
            b'\n' if idx + 1 < payload.len() && payload[idx + 1] == b'\n' => return false,
            _ => {}
        }
        idx += 1;
    }
    // No "\n\n" separator: a body-less tag is fine as long as the last header line
    // is newline-terminated.
    if payload.last() == Some(&b'\n') {
        return false;
    }
    reporter.report(
        FsckSeverity::Error,
        "unterminatedHeader",
        "unterminated header",
    );
    true
}

/// Walks the header lines of a tag payload one `\n`-terminated line at a time,
/// stopping at the blank line that separates the headers from the message.
struct LineCursor<'a> {
    payload: &'a [u8],
    /// Byte offset of the next unread line.
    pos: usize,
    /// Start offset of the line returned by the most recent `next_header_line`,
    /// so a caller can `rewind_last` when it decides not to consume that line.
    last_line_start: usize,
}

impl<'a> LineCursor<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self {
            payload,
            pos: 0,
            last_line_start: 0,
        }
    }

    /// Return the next header line's content (without the trailing `\n`), or
    /// `None` at the blank line / end of the header region. Advances past the
    /// line and its newline.
    fn next_header_line(&mut self) -> Option<&'a [u8]> {
        self.last_line_start = self.pos;
        if self.pos >= self.payload.len() {
            return None;
        }
        // A blank line (the header/message separator) terminates the headers.
        if self.payload[self.pos] == b'\n' {
            return None;
        }
        let rest = &self.payload[self.pos..];
        match rest.iter().position(|byte| *byte == b'\n') {
            Some(idx) => {
                self.pos += idx + 1;
                Some(&rest[..idx])
            }
            None => {
                // No trailing newline: the remainder is the (unterminated) line.
                // verify_headers already flagged this; return it so prefix
                // matching can still proceed.
                self.pos = self.payload.len();
                Some(rest)
            }
        }
    }

    /// Undo the most recent `next_header_line`, so the next call returns the same
    /// line. Used when a missing optional header (the tagger) means git did not
    /// actually consume the line it inspected.
    fn rewind_last(&mut self) {
        self.pos = self.last_line_start;
    }

    /// Skip an optional `gpgsig`/`gpgsig-sha256` header and its folded
    /// continuation lines (those beginning with a space), matching git's signed-
    /// tag handling. A no-op when no such header is present at the cursor.
    fn skip_gpgsig_header(&mut self) {
        let Some(rest) = self.remaining() else {
            return;
        };
        if !rest.starts_with(b"gpgsig ") && !rest.starts_with(b"gpgsig-sha256 ") {
            return;
        }
        // Consume the gpgsig line.
        let Some(line) = self.next_header_line() else {
            return;
        };
        let _ = line;
        // Consume folded continuation lines (start with a space). A continuation
        // line is never the blank separator, so next_header_line returns it.
        while self
            .remaining()
            .is_some_and(|rest| rest.first() == Some(&b' '))
        {
            if self.next_header_line().is_none() {
                break;
            }
        }
    }

    /// The unread remainder of the payload, or `None` at end of input.
    fn remaining(&self) -> Option<&'a [u8]> {
        self.payload.get(self.pos..).filter(|rest| !rest.is_empty())
    }

    /// True when, after the consumed headers, another header line (rather than the
    /// blank separator or end of input) remains.
    fn has_extra_header(&self) -> bool {
        match self.payload.get(self.pos) {
            None => false,
            // The header/message separator is a blank line; anything else is extra.
            Some(byte) => *byte != b'\n',
        }
    }
}

/// Strip a literal header `prefix` (e.g. `b"object "`, including its single
/// trailing space, matching git's `skip_prefix`) from `line`, returning the
/// remainder, or `None` when `line` does not start with exactly that prefix.
fn strip_header_prefix<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    line.strip_prefix(prefix)
}

/// Parse an `object` line value: it must be exactly `hex_len` hex digits (upper-
/// or lowercase, as git's `parse_oid_hex` accepts) with nothing trailing.
fn parse_oid_line(format: ObjectFormat, value: &[u8]) -> Option<ObjectId> {
    if value.len() != format.hex_len() {
        return None;
    }
    if !value.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let text = std::str::from_utf8(value).ok()?;
    ObjectId::from_hex(format, text).ok()
}

/// Parse a `type` line value into an [`ObjectType`]. The value is the entire
/// remainder of the line, so any stray content (a trailing space, mixed case)
/// makes it unknown, matching git's `type_from_string_gently`.
fn parse_object_type(value: &[u8]) -> Option<ObjectType> {
    match value {
        b"blob" => Some(ObjectType::Blob),
        b"tree" => Some(ObjectType::Tree),
        b"commit" => Some(ObjectType::Commit),
        b"tag" => Some(ObjectType::Tag),
        _ => None,
    }
}

/// git's `fsck_ident` over the value following `tagger ` (which ends at the line's
/// `\n`). Reports at most one problem (always error-severity) and returns whether
/// it reported one.
fn fsck_ident(ident: &[u8], reporter: &mut FsckReporter) -> bool {
    match ident_problem(ident) {
        Some((id, detail)) => reporter.report(FsckSeverity::Error, id, detail),
        None => false,
    }
}

/// The single fsck problem git's `fsck_ident` would report for `ident` (the text
/// following `tagger `, without its trailing `\n`), as `(msgid, detail)`, or
/// `None` for a well-formed identity. The check order and byte-level boundaries
/// mirror fsck.c exactly; all identity problems are error-severity, so the caller
/// supplies the severity. Keeping the decision pure lets it be unit-tested
/// directly while [`fsck_ident`] handles the reporting side effect.
fn ident_problem(ident: &[u8]) -> Option<(&'static str, &'static str)> {
    // An identity that begins with '<' has no name at all. (git's message text
    // here is the same "missing space before email" string.)
    if ident.first() == Some(&b'<') {
        return Some((
            "missingNameBeforeEmail",
            "invalid author/committer line - missing space before email",
        ));
    }

    // Scan the name region: like git's loop, stop at '>' (badName), end-of-line
    // (missingEmail), or '<' (end of name).
    let mut pos = 0;
    loop {
        match ident.get(pos) {
            None => {
                return Some((
                    "missingEmail",
                    "invalid author/committer line - missing email",
                ));
            }
            Some(&b'>') => return Some(("badName", "invalid author/committer line - bad name")),
            Some(&b'<') => break,
            Some(_) => pos += 1,
        }
    }
    // The character immediately before '<' must be a space.
    if pos == 0 || ident[pos - 1] != b' ' {
        return Some((
            "missingSpaceBeforeEmail",
            "invalid author/committer line - missing space before email",
        ));
    }

    // Email content runs to '>', rejecting an embedded '<' or end-of-line.
    pos += 1; // past '<'
    loop {
        match ident.get(pos) {
            None | Some(&b'<') => {
                return Some(("badEmail", "invalid author/committer line - bad email"));
            }
            Some(&b'>') => break,
            Some(_) => pos += 1,
        }
    }
    pos += 1; // past '>'

    // A single space must separate the email from the date (a tab does not count).
    if ident.get(pos) != Some(&b' ') {
        return Some((
            "missingSpaceBeforeDate",
            "invalid author/committer line - missing space before date",
        ));
    }
    pos += 1;
    // git then skips any further linear whitespace (spaces and tabs) before the
    // date, having traditionally tolerated extra whitespace.
    while matches!(ident.get(pos), Some(&b' ') | Some(&b'\t')) {
        pos += 1;
    }

    // The date must start with a digit.
    if !matches!(ident.get(pos), Some(byte) if byte.is_ascii_digit()) {
        return Some(("badDate", "invalid author/committer line - bad date"));
    }
    // A leading zero on a multi-digit date (i.e. next byte is not a space) is
    // zero-padded. (git checks `*p == '0' && p[1] != ' '`.)
    if ident.get(pos) == Some(&b'0') && ident.get(pos + 1) != Some(&b' ') {
        return Some((
            "zeroPaddedDate",
            "invalid author/committer line - zero-padded date",
        ));
    }
    // Consume the digit run and check for overflow (git caps the timestamp at
    // TIME_MAX, i.e. i64::MAX; anything larger overflows).
    let date_start = pos;
    while matches!(ident.get(pos), Some(byte) if byte.is_ascii_digit()) {
        pos += 1;
    }
    if date_overflows(&ident[date_start..pos]) {
        return Some((
            "badDateOverflow",
            "invalid author/committer line - date causes integer overflow",
        ));
    }
    // After the date digits, a space then the timezone.
    if ident.get(pos) != Some(&b' ') {
        return Some(("badDate", "invalid author/committer line - bad date"));
    }
    pos += 1;

    // The timezone is a sign followed by exactly four digits, then end-of-line
    // (here, end of the value).
    if !is_valid_timezone(&ident[pos..]) {
        return Some((
            "badTimezone",
            "invalid author/committer line - bad time zone",
        ));
    }

    None
}

/// Whether a run of ASCII digits represents a timestamp that git would consider
/// overflowing. git stores the timestamp in a signed 64-bit `timestamp_t`, capped
/// at `TIME_MAX` (`i64::MAX`), and additionally caps the parse buffer at 23
/// digits; any value above `i64::MAX` (which includes anything with more digits
/// than `i64::MAX`) overflows.
fn date_overflows(digits: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(digits) else {
        return true;
    };
    match text.parse::<u128>() {
        Ok(value) => value > i64::MAX as u128,
        // More digits than a u128 can hold is certainly an overflow.
        Err(_) => true,
    }
}

/// A timezone is `[+-]` followed by exactly four ASCII digits and nothing else
/// (the remainder of the tagger line). Matches git's tz validation in fsck_ident.
fn is_valid_timezone(tz: &[u8]) -> bool {
    tz.len() == 5 && (tz[0] == b'+' || tz[0] == b'-') && tz[1..].iter().all(u8::is_ascii_digit)
}

/// git's `check_refname_format("refs/tags/<name>", 0)` as applied to a tag name.
/// Because git prefixes `refs/tags/`, the name is validated as a (possibly multi-
/// level) refname: split on `/`, every component must be non-empty and valid.
///
/// Returns true when `name` is a valid tag name. Rules (refs.c
/// `check_refname_component`):
///   * Components are `/`-separated; none may be empty (so no leading/trailing
///     slash and no `//`).
///   * A component may not start with `.`, end with `.`, end with `.lock`, or
///     contain `..`.
///   * No component may contain a control byte (`< 0x20`), `0x7f` (DEL), space,
///     or any of `~ ^ : ? * [ \`, nor the two-byte sequence `@{`.
fn check_refname_format(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    name.split(|byte| *byte == b'/')
        .all(check_refname_component)
}

/// Validate a single refname component per git's `check_refname_component`.
fn check_refname_component(component: &[u8]) -> bool {
    if component.is_empty() {
        return false;
    }
    if component.first() == Some(&b'.') {
        return false;
    }
    if component.last() == Some(&b'.') {
        return false;
    }
    if component.ends_with(b".lock") {
        return false;
    }
    for (idx, &byte) in component.iter().enumerate() {
        match byte {
            // Control characters, space, and DEL are disallowed.
            0x00..=0x20 | 0x7f => return false,
            // Refspec/pathspec metacharacters disallowed in refnames.
            b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\' => return false,
            // No ".." within a component.
            b'.' if component.get(idx + 1) == Some(&b'.') => return false,
            // No "@{" sequence within a refname.
            b'@' if component.get(idx + 1) == Some(&b'{') => return false,
            _ => {}
        }
    }
    true
}

/// Verify the tagged object exists and its actual type matches the declared
/// `type` line. These reports are *not* wrapped in the fsck framing and use the
/// canonical (lowercase) object id.
///
///   * A missing/unreadable object: `fatal: could not read tagged object '<oid>'`.
///   * A type mismatch:
///     `fatal: object '<oid>' tagged as '<declared>', but is a '<actual>' type`.
fn verify_tagged_object(
    replace_objects: bool,
    repo: &sley::Repository,
    parsed: &ParsedTag,
) -> Result<()> {
    let refs = repo.references();
    let read_oid = apply_replace_object(replace_objects, &refs, &parsed.object_id)?;
    let object = match repo.read_object(&read_oid) {
        Ok(object) => object,
        Err(_) => {
            eprintln!("fatal: could not read tagged object '{}'", parsed.object_id);
            return Err(GitError::Exit(128));
        }
    };
    if object.object_type != parsed.declared_type {
        eprintln!(
            "fatal: object '{}' tagged as '{}', but is a '{}' type",
            parsed.object_id,
            parsed.declared_type.as_str(),
            object.object_type.as_str()
        );
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// git's standard "not a git repository" fatal, emitted before reading stdin's
/// contents when discovery fails. Exit 128.
fn mktag_not_a_repository() -> Result<()> {
    eprintln!("fatal: not a git repository (or any of the parent directories): .git");
    Err(GitError::Exit(128))
}

fn mktag_unknown_option_error(option: &str) -> Result<MktagInvocation> {
    eprintln!("error: unknown option `{option}'");
    eprint!("{MKTAG_USAGE}");
    Err(GitError::Exit(129))
}

fn mktag_unknown_switch_error(switch: char) -> Result<MktagInvocation> {
    eprintln!("error: unknown switch `{switch}'");
    eprint!("{MKTAG_USAGE}");
    Err(GitError::Exit(129))
}

fn mktag_option_takes_no_value_error(option: &str) -> Result<MktagInvocation> {
    // git's parse-options prints only the error for a "takes no value" rejection,
    // without the usage block (unlike the unknown-option/switch errors above).
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

/// The trailing fatal line printed after any fsck problem that aborts `mktag`.
const FSCK_FATAL_TEXT: &str = "fatal: tag on stdin did not pass our strict fsck check";

/// The exact usage block git prints for `mktag` (stdout for `-h`, stderr after an
/// option error). Reproduced byte-for-byte, including the trailing blank line.
const MKTAG_USAGE: &str = "\
usage: git mktag

    --[no-]strict         enable more strict checking

";

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn sha1_oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, hex).expect("valid sha1 hex")
    }

    /// Build a payload from a header block (without its trailing newline) and a
    /// message body, joined by the blank-line separator.
    fn payload(headers: &str, message: &str) -> Vec<u8> {
        format!("{headers}\n\n{message}").into_bytes()
    }

    /// Run the fsck capturing the rendered lines (without the trailing fatal),
    /// the fatal flag, and whether structural headers parsed. The reporter prints
    /// to stderr, so this re-implements the policy decisions by inspecting the
    /// returned reporter state and a recording reporter is unnecessary: instead
    /// we drive the real `fsck_tag` and read back `is_fatal`.
    struct FsckRun {
        fatal: bool,
        parsed: bool,
    }

    fn run_fsck(payload: &[u8], strict: bool) -> FsckRun {
        let mut reporter = FsckReporter::new(strict);
        let parsed = fsck_tag(ObjectFormat::Sha1, payload, &mut reporter);
        FsckRun {
            fatal: reporter.is_fatal(),
            parsed: parsed.is_some(),
        }
    }

    const OBJ: &str = "066c3b43d8b3916ee290e6416995e79a82583a80";
    const TAGGER: &str = "tagger Tester <tester@example.com> 1790000000 -0500";

    // ----- argument parsing -----

    #[test]
    fn parses_default_strict() {
        match parse_mktag_args(&[]).expect("parse") {
            MktagInvocation::Run { strict } => assert!(strict),
            MktagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn parses_no_strict_toggle() {
        match parse_mktag_args(&["--no-strict".to_string()]).expect("parse") {
            MktagInvocation::Run { strict } => assert!(!strict),
            MktagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn last_strict_toggle_wins() {
        let args = vec!["--no-strict".to_string(), "--strict".to_string()];
        match parse_mktag_args(&args).expect("parse") {
            MktagInvocation::Run { strict } => assert!(strict),
            MktagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn help_flags_request_help() {
        for flag in ["-h", "--help", "--help-all"] {
            assert_eq!(
                parse_mktag_args(&[flag.to_string()]).expect("parse"),
                MktagInvocation::Help
            );
        }
    }

    #[test]
    fn double_dash_then_operand_ignored() {
        let args = vec!["--".to_string(), "--strict".to_string()];
        match parse_mktag_args(&args).expect("parse") {
            MktagInvocation::Run { strict } => assert!(strict),
            MktagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn end_of_options_then_operand_ignored() {
        let args = vec!["--end-of-options".to_string(), "--bogus".to_string()];
        match parse_mktag_args(&args).expect("parse") {
            MktagInvocation::Run { strict } => assert!(strict),
            MktagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn bare_operand_is_ignored() {
        match parse_mktag_args(&["extra".to_string()]).expect("parse") {
            MktagInvocation::Run { strict } => assert!(strict),
            MktagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn unknown_long_option_is_exit_129() {
        match parse_mktag_args(&["--bogus".to_string()]) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }

    #[test]
    fn unknown_short_switch_is_exit_129() {
        match parse_mktag_args(&["-z".to_string()]) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }

    #[test]
    fn strict_with_value_is_exit_129() {
        match parse_mktag_args(&["--strict=1".to_string()]) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }

    // ----- valid payloads -----

    #[test]
    fn valid_payload_passes_and_parses() {
        let p = payload(
            &format!("object {OBJ}\ntype commit\ntag v1.0\n{TAGGER}"),
            "m\n",
        );
        let run = run_fsck(&p, true);
        assert!(!run.fatal);
        assert!(run.parsed);
    }

    #[test]
    fn uppercase_object_sha_is_accepted() {
        let upper = OBJ.to_ascii_uppercase();
        let p = payload(
            &format!("object {upper}\ntype commit\ntag v1.0\n{TAGGER}"),
            "m\n",
        );
        assert!(!run_fsck(&p, true).fatal);
    }

    #[test]
    fn gpgsig_header_is_not_extra() {
        // A gpgsig header (with folded continuation) after the tagger is allowed.
        let p = payload(
            &format!(
                "object {OBJ}\ntype commit\ntag v1.0\n{TAGGER}\ngpgsig -----BEGIN-----\n -----END-----"
            ),
            "m\n",
        );
        assert!(!run_fsck(&p, true).fatal);
    }

    // ----- structural fsck (fatal in both modes) -----

    #[test]
    fn structural_errors_are_fatal_in_both_modes() {
        let cases = [
            payload(&format!("type commit\ntag v1.0\n{TAGGER}"), "m\n"),
            payload(&format!("object {OBJ}\ntag v1.0\n{TAGGER}"), "m\n"),
            payload(&format!("object {OBJ}\ntype commit\n{TAGGER}"), "m\n"),
            payload(
                &format!("object zzzz\ntype commit\ntag v1.0\n{TAGGER}"),
                "m\n",
            ),
            payload(
                &format!("object {OBJ} \ntype commit\ntag v1.0\n{TAGGER}"),
                "m\n",
            ),
            payload(
                &format!("object {OBJ}a\ntype commit\ntag v1.0\n{TAGGER}"),
                "m\n",
            ),
            payload(
                &format!("object {OBJ}\ntype bogus\ntag v1.0\n{TAGGER}"),
                "m\n",
            ),
            payload(
                &format!("Object {OBJ}\ntype commit\ntag v1.0\n{TAGGER}"),
                "m\n",
            ),
            format!("object {OBJ}\ntype commit\ntag v1").into_bytes(),
            Vec::new(),
        ];
        for case in &cases {
            assert!(
                run_fsck(case, true).fatal,
                "strict should be fatal: {case:?}"
            );
            assert!(
                run_fsck(case, false).fatal,
                "non-strict should be fatal: {case:?}"
            );
        }
    }

    #[test]
    fn structural_failure_does_not_parse() {
        let p = payload(
            &format!("object {OBJ}\ntype bogus\ntag v1.0\n{TAGGER}"),
            "m\n",
        );
        assert!(!run_fsck(&p, true).parsed);
    }

    #[test]
    fn nul_in_body_is_allowed_but_header_nul_is_not() {
        let mut body = payload(
            &format!("object {OBJ}\ntype commit\ntag v1.0\n{TAGGER}"),
            "bo",
        );
        body.push(0);
        body.extend_from_slice(b"dy\n");
        assert!(!run_fsck(&body, true).fatal);

        let mut header_nul = format!("object {OBJ}\ntype commit\ntag v").into_bytes();
        header_nul.push(0);
        header_nul.extend_from_slice(format!("1\n{TAGGER}\n\nm\n").as_bytes());
        assert!(run_fsck(&header_nul, false).fatal);
    }

    // ----- severity & the report-overwrite control flow -----

    #[test]
    fn bad_tag_name_alone_is_warning_in_non_strict() {
        // badTagName is the last reported problem (tagger is present and valid),
        // so non-strict writes the tag.
        let p = payload(
            &format!("object {OBJ}\ntype commit\ntag a:b\n{TAGGER}"),
            "m\n",
        );
        assert!(!run_fsck(&p, false).fatal);
        assert!(run_fsck(&p, true).fatal);
    }

    #[test]
    fn missing_tagger_alone_is_warning_in_non_strict() {
        let p = payload(&format!("object {OBJ}\ntype commit\ntag v1.0"), "m\n");
        assert!(!run_fsck(&p, false).fatal);
        assert!(run_fsck(&p, true).fatal);
    }

    #[test]
    fn ident_error_alone_is_fatal_in_both_modes() {
        // No trailing warning to mask it: badTimezone is the final report.
        let p = payload(
            &format!("object {OBJ}\ntype commit\ntag v1.0\ntagger N <e@x> 1 0500"),
            "m\n",
        );
        assert!(run_fsck(&p, false).fatal);
        assert!(run_fsck(&p, true).fatal);
    }

    #[test]
    fn trailing_warning_masks_ident_error_in_non_strict() {
        // The git quirk: an identity error followed by extraHeaderEntry — the
        // warning's report overwrites the error's, so non-strict writes the tag.
        let p = payload(
            &format!("object {OBJ}\ntype commit\ntag v1.0\ntagger N <e@x> 1 0500\nextra"),
            "m\n",
        );
        assert!(!run_fsck(&p, false).fatal, "non-strict should be masked");
        // Strict still aborts (every report is non-zero).
        assert!(run_fsck(&p, true).fatal);
    }

    #[test]
    fn extra_header_after_tagger_alone_is_warning_in_non_strict() {
        let p = payload(
            &format!("object {OBJ}\ntype commit\ntag v1.0\n{TAGGER}\nfoo bar"),
            "m\n",
        );
        assert!(!run_fsck(&p, false).fatal);
        assert!(run_fsck(&p, true).fatal);
    }

    // ----- refname format -----

    #[test]
    fn refname_rules_match_git() {
        for name in [
            "v1.0", "a/b", "foo.bar", "x.lock.y", "@", "@@", "a@", "-bad", "HEAD",
        ] {
            assert!(
                check_refname_format(name.as_bytes()),
                "{name} should be accepted"
            );
        }
        for name in [
            "",
            "has space",
            "a..b",
            "foo.lock",
            "a/b.lock",
            "ends/with.lock",
            ".foo",
            "foo.",
            "a.",
            "/foo",
            "foo/",
            "a//b",
            "a@{b",
            "a~b",
            "a^b",
            "a:b",
            "a?b",
            "a*b",
            "a[b",
            "a\\b",
            ".",
        ] {
            assert!(
                !check_refname_format(name.as_bytes()),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn refname_rejects_control_and_del() {
        assert!(!check_refname_format(b"a\tb"));
        assert!(!check_refname_format(b"a\x7fb"));
        assert!(!check_refname_format(&[b'a', 0x01, b'b']));
    }

    // ----- ident -----

    /// The id of the single problem the production `ident_problem` reports.
    fn ident_id(ident: &str) -> Option<&'static str> {
        ident_problem(ident.as_bytes()).map(|(id, _)| id)
    }

    #[test]
    fn ident_boundary_cases_match_git() {
        let cases: &[(&str, Option<&str>)] = &[
            ("Tester <tester@example.com> 1790000000 -0500", None),
            (
                "<tester@example.com> 1 -0500",
                Some("missingNameBeforeEmail"),
            ),
            (
                "ab<tester@example.com> 1 -0500",
                Some("missingSpaceBeforeEmail"),
            ),
            ("Tester>x <t@e.co> 1 -0500", Some("badName")),
            ("Tester 1 -0500", Some("missingEmail")),
            ("ab <t@e.co 1 -0500", Some("badEmail")),
            ("ab <a<b@e.co> 1 -0500", Some("badEmail")),
            ("Tester <t@e.co>1 -0500", Some("missingSpaceBeforeDate")),
            ("Tester <t@e.co>\t1 -0500", Some("missingSpaceBeforeDate")),
            ("Tester <t@e.co> abc -0500", Some("badDate")),
            ("Tester <t@e.co> 12x -0500", Some("badDate")),
            ("Tester <t@e.co> 007 -0500", Some("zeroPaddedDate")),
            ("Tester <t@e.co> 00 -0500", Some("zeroPaddedDate")),
            ("Tester <t@e.co> 1 0500", Some("badTimezone")),
            ("Tester <t@e.co> 1 +050", Some("badTimezone")),
            ("Tester <t@e.co> 1 +05000", Some("badTimezone")),
            ("Tester <t@e.co> 1 +05ab", Some("badTimezone")),
            ("Tester <t@e.co> 1 +0500x", Some("badTimezone")),
            // Two spaces before the date are tolerated.
            ("Tester <t@e.co>  1790000000 -0500", None),
            // Empty name (a leading space becomes the name) is accepted.
            (" <t@e.co> 1 -0500", None),
            // Empty email is accepted.
            ("Tester <> 1 -0500", None),
            // Single-zero date is accepted.
            ("Tester <t@e.co> 0 +0000", None),
            // i64::MAX is the boundary: accepted; +1 overflows.
            ("Tester <t@e.co> 9223372036854775807 +0000", None),
            (
                "Tester <t@e.co> 9223372036854775808 +0000",
                Some("badDateOverflow"),
            ),
            (
                "Tester <t@e.co> 18446744073709551615 +0000",
                Some("badDateOverflow"),
            ),
        ];
        for (ident, expected) in cases {
            assert_eq!(ident_id(ident), *expected, "ident {ident:?}");
        }
    }

    #[test]
    fn timezone_validation() {
        for tz in ["+0000", "-0500", "+9999"] {
            assert!(is_valid_timezone(tz.as_bytes()), "{tz} should be valid");
        }
        for tz in ["0500", "+050", "+05000", "+05ab", ""] {
            assert!(!is_valid_timezone(tz.as_bytes()), "{tz} should be invalid");
        }
    }

    #[test]
    fn date_overflow_threshold_is_i64_max() {
        assert!(!date_overflows(b"9223372036854775807")); // i64::MAX
        assert!(date_overflows(b"9223372036854775808")); // i64::MAX + 1
        assert!(date_overflows(b"18446744073709551615")); // u64::MAX
        assert!(!date_overflows(b"0"));
        assert!(!date_overflows(b"1790000000"));
        // More digits than u128 can hold is still an overflow, not a panic.
        assert!(date_overflows(&[b'9'; 40]));
    }

    // ----- parsing helpers -----

    #[test]
    fn oid_line_parsing() {
        assert_eq!(
            parse_oid_line(ObjectFormat::Sha1, OBJ.as_bytes()),
            Some(sha1_oid(OBJ))
        );
        assert!(parse_oid_line(ObjectFormat::Sha1, b"1234").is_none());
        assert!(
            parse_oid_line(
                ObjectFormat::Sha1,
                b"zzzz3b43d8b3916ee290e6416995e79a82583a80"
            )
            .is_none()
        );
        // Uppercase parses to the same canonical id as lowercase.
        assert_eq!(
            parse_oid_line(ObjectFormat::Sha1, OBJ.to_ascii_uppercase().as_bytes()),
            Some(sha1_oid(OBJ))
        );
    }

    #[test]
    fn object_type_parsing() {
        assert_eq!(parse_object_type(b"blob"), Some(ObjectType::Blob));
        assert_eq!(parse_object_type(b"tree"), Some(ObjectType::Tree));
        assert_eq!(parse_object_type(b"commit"), Some(ObjectType::Commit));
        assert_eq!(parse_object_type(b"tag"), Some(ObjectType::Tag));
        assert_eq!(parse_object_type(b"Commit"), None);
        assert_eq!(parse_object_type(b"commit "), None);
    }

    #[test]
    fn parsed_tag_uses_canonical_oid() {
        // Even an uppercase payload SHA yields the canonical (lowercase) id used
        // for the tagged-object error messages.
        let upper = OBJ.to_ascii_uppercase();
        let p = payload(
            &format!("object {upper}\ntype commit\ntag v1.0\n{TAGGER}"),
            "m\n",
        );
        let mut reporter = FsckReporter::new(true);
        let parsed = fsck_tag(ObjectFormat::Sha1, &p, &mut reporter).expect("parsed");
        assert_eq!(parsed.object_id, sha1_oid(OBJ));
        assert_eq!(parsed.object_id.to_string(), OBJ);
        assert_eq!(parsed.declared_type, ObjectType::Commit);
    }
}
