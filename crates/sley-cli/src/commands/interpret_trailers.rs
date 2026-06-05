//! `git interpret-trailers` — parse and edit the trailer block of a commit
//! message.
//!
//! This is a self-contained, pure-text command: it reads one or more messages
//! (from files or stdin), locates the *trailer block* (the run of `Key: value`
//! lines at the bottom of the message), applies the requested edits, and prints
//! the result. Unlike most commands here it does **not** require a repository —
//! real `git interpret-trailers` happily runs outside one (exit 0). We therefore
//! only consult repository/`-c` configuration on a best-effort basis.
//!
//! The behaviour reproduced here mirrors `trailer.c` from the reference git:
//!
//!   * Trailer-block detection (`find_trailer_block_start`): the block is the
//!     final paragraph of the message body (everything after the last blank
//!     line) provided that paragraph is not the whole message. A paragraph is a
//!     trailer block when **either** every line is a trailer/continuation/
//!     comment (no "non-trailer" lines) **or** it contains a git-generated
//!     prefix (`Signed-off-by: ` / `(cherry picked from commit `) and at least
//!     25% of its lines are trailers (`trailer_lines * 3 >= non_trailer_lines`).
//!   * A line is a trailer when it has a separator (default `:`) whose token
//!     part is non-empty and free of whitespace; a line starting with
//!     whitespace is a continuation of the previous trailer; a comment line
//!     (default `#`) is ignored.
//!   * The `---` patch divider (a line equal to `---` followed by whitespace)
//!     ends the message; anything from there on is preserved verbatim and the
//!     trailer block is sought in the text *before* it (suppressed by
//!     `--no-divider`).
//!   * `--trailer <key>[(=|:)<value>]` queues a trailer to apply. The argument
//!     separator is the first character of the configured separator set (`=` is
//!     always accepted in addition); the output separator is the first
//!     character of `trailer.separators` (default `:`), always followed by a
//!     space.
//!   * Placement (`--where start|end|after|before`, default `end`) and the
//!     duplicate policies `--if-exists`
//!     (`addIfDifferent`/`addIfDifferentNeighbor`/`replace`/`doNothing`/`add`,
//!     default `addIfDifferentNeighbor`) and `--if-missing` (`add`/`doNothing`,
//!     default `add`) drive how each queued trailer merges into the block.
//!   * `--only-trailers` prints just the trailers; `--only-input` keeps the
//!     parsed input trailers untouched by applied args/config; `--unfold`
//!     collapses multi-line values to one space-joined line; `--parse` is the
//!     documented alias for `--only-trailers --only-input --unfold`.
//!   * `--trim-empty` drops trailers whose value is empty; `--in-place` rewrites
//!     each input file instead of streaming to stdout.
//!
//! Output formatting matches git byte-for-byte: values are whitespace-trimmed,
//! the output separator is normalised, an empty value is rendered as the
//! separator plus a trailing space, and a blank line is inserted between a
//! non-empty body and a freshly created trailer block.
//!
//! This module follows the glob-import + private-helper structure of the other
//! self-contained command modules (`commands::stash`, `commands::tag`,
//! `commands::verify_commit`); see `commands::stash` for the rationale behind the
//! wildcard import.

// Glob the crate root for shared plumbing (discover_git_dir, read_repo_config,
// global_config_value, GitError, Result, io, fs, env, the Read/Write traits,
// Path/PathBuf, etc.); see commands::stash for why this is a wildcard.
use crate::*;

/// Usage text, byte-for-byte identical to the reference git. Printed to stdout
/// for `-h`/`--help` and to stderr after an option-parse error (both exit 129).
const USAGE: &str = "\
usage: git interpret-trailers [--in-place] [--trim-empty]
                              [(--trailer (<key>|<key-alias>)[(=|:)<value>])...]
                              [--parse] [<file>...]

    --[no-]in-place       edit files in place
    --[no-]trim-empty     trim empty trailers
    --[no-]where <placement>
                          where to place the new trailer
    --[no-]if-exists <action>
                          action if trailer already exists
    --[no-]if-missing <action>
                          action if trailer is missing
    --[no-]only-trailers  output only the trailers
    --[no-]only-input     do not apply trailer.<key-alias> configuration variables
    --[no-]unfold         reformat multiline trailer values as single-line values
    --parse               alias for --only-trailers --only-input --unfold
    --no-divider          do not treat \"---\" as the end of input
    --divider             opposite of --no-divider
    --[no-]trailer <trailer>
                          trailer(s) to add

";

/// Where a freshly applied trailer is placed relative to existing ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Where {
    End,
    Start,
    After,
    Before,
}

/// What to do when a trailer with the same token already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IfExists {
    AddIfDifferentNeighbor,
    AddIfDifferent,
    Add,
    Replace,
    DoNothing,
}

/// What to do when no trailer with the same token exists yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IfMissing {
    Add,
    DoNothing,
}

/// A trailer queued by `--trailer`. `token`/`value` are already split on the
/// argument separator but not yet whitespace-normalised for output.
#[derive(Debug, Clone)]
struct ArgTrailer {
    token: String,
    value: String,
    where_: Where,
    if_exists: IfExists,
    if_missing: IfMissing,
}

/// Fully parsed command-line options.
#[derive(Debug)]
struct Options {
    in_place: bool,
    trim_empty: bool,
    only_trailers: bool,
    only_input: bool,
    unfold: bool,
    no_divider: bool,
    /// Default placement/policy for trailers that don't override them.
    default_where: Where,
    default_if_exists: IfExists,
    default_if_missing: IfMissing,
    /// Output separator character (first of `trailer.separators`, default ':').
    out_separator: char,
    /// Set of characters that separate a token from its value when parsing.
    separators: Vec<char>,
    /// Comment-line prefix (default '#').
    comment_prefix: String,
    trailers: Vec<ArgTrailer>,
    files: Vec<String>,
}

/// Outcome of argument parsing: either run with options, or print help.
enum Invocation {
    Run(Box<Options>),
    Help,
}

/// Entry point for `git interpret-trailers`.
pub(crate) fn cmd_interpret_trailers(args: &[String]) -> Result<()> {
    let options = match parse_args(args)? {
        Invocation::Run(options) => options,
        Invocation::Help => {
            print!("{USAGE}");
            io::stdout().flush()?;
            return Err(GitError::Exit(129));
        }
    };

    // git rejects queueing `--trailer` while `--only-input` is in effect (also
    // reached via `--parse`, which implies `--only-input`): the queued trailers
    // would never be applied. Diagnostic + usage on stderr, exit 129.
    if options.only_input && !options.trailers.is_empty() {
        eprintln!("fatal: --trailer with --only-input does not make sense");
        eprintln!();
        eprint!("{USAGE}");
        return Err(GitError::Exit(129));
    }

    if options.files.is_empty() {
        // No file operands: read the single message from stdin and stream the
        // result to stdout. `--in-place` is meaningless without files (git
        // simply ignores it here, treating stdin as the source).
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        let text = String::from_utf8_lossy(&input).into_owned();
        let rendered = process_message(&text, &options);
        let mut stdout = io::stdout();
        stdout.write_all(rendered.as_bytes())?;
        stdout.flush()?;
        return Ok(());
    }

    // With file operands, process each in turn. Without `--in-place` the
    // rendered messages are concatenated to stdout in argument order (git emits
    // no separator between them); with `--in-place` each file is rewritten.
    let mut stdout_buf = String::new();
    for file in &options.files {
        let bytes = match fs::read(file) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!(
                    "fatal: could not read input file '{file}': {}",
                    io_reason(&err)
                );
                return Err(GitError::Exit(128));
            }
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let rendered = process_message(&text, &options);
        if options.in_place {
            fs::write(file, rendered.as_bytes())?;
        } else {
            stdout_buf.push_str(&rendered);
        }
    }
    if !options.in_place {
        let mut stdout = io::stdout();
        stdout.write_all(stdout_buf.as_bytes())?;
        stdout.flush()?;
    }
    Ok(())
}

/// Render a libc-style `strerror` reason for the "could not read input file"
/// fatal, matching git which formats with the OS error string.
fn io_reason(err: &std::io::Error) -> String {
    match err.raw_os_error() {
        Some(code) => {
            // std's Display for an OS error is "<message> (os error N)"; git
            // prints only the message, so strip the parenthetical suffix.
            let full = std::io::Error::from_raw_os_error(code).to_string();
            match full.rfind(" (os error ") {
                Some(idx) => full[..idx].to_string(),
                None => full,
            }
        }
        None => err.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Parse argv into [`Options`]. On an option error this prints git's diagnostic
/// to stderr and returns `Err(GitError::Exit(129))`; `-h`/`--help` yields
/// [`Invocation::Help`].
fn parse_args(args: &[String]) -> Result<Invocation> {
    // Seed defaults from configuration (best-effort) before applying argv.
    let config = load_trailer_config();

    let mut opts = Options {
        in_place: false,
        trim_empty: false,
        only_trailers: false,
        only_input: false,
        unfold: false,
        no_divider: false,
        default_where: config.where_,
        default_if_exists: config.if_exists,
        default_if_missing: config.if_missing,
        out_separator: config.out_separator,
        separators: config.separators,
        comment_prefix: config.comment_prefix,
        trailers: Vec::new(),
        files: Vec::new(),
    };

    // Each queued `--trailer` captures the placement/policy in force *at the
    // time it appears*, so a later `--where`/`--if-exists`/`--if-missing` only
    // affects subsequent trailers (matching git's per-arg conf snapshot).
    let mut cur_where = opts.default_where;
    let mut cur_if_exists = opts.default_if_exists;
    let mut cur_if_missing = opts.default_if_missing;

    let mut idx = 0;
    let mut only_positional = false;
    while idx < args.len() {
        let arg = &args[idx];
        if only_positional {
            opts.files.push(arg.clone());
            idx += 1;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "--" => {
                only_positional = true;
            }
            "--in-place" => opts.in_place = true,
            "--no-in-place" => opts.in_place = false,
            "--trim-empty" => opts.trim_empty = true,
            "--no-trim-empty" => opts.trim_empty = false,
            "--only-trailers" => opts.only_trailers = true,
            "--no-only-trailers" => opts.only_trailers = false,
            "--only-input" => opts.only_input = true,
            "--no-only-input" => opts.only_input = false,
            "--unfold" => opts.unfold = true,
            "--no-unfold" => opts.unfold = false,
            "--no-divider" => opts.no_divider = true,
            "--divider" => opts.no_divider = false,
            "--parse" => {
                // Documented alias for --only-trailers --only-input --unfold.
                opts.only_trailers = true;
                opts.only_input = true;
                opts.unfold = true;
            }
            _ => {
                // Value-bearing options, in both `--opt value` and `--opt=value`
                // spellings.
                if let Some(value) = match_value_option(args, &mut idx, "--trailer")? {
                    let trailer = parse_trailer_arg(
                        &value,
                        &opts.separators,
                        cur_where,
                        cur_if_exists,
                        cur_if_missing,
                    );
                    opts.trailers.push(trailer);
                } else if let Some(value) = match_value_option(args, &mut idx, "--where")? {
                    match parse_where(&value) {
                        Some(w) => cur_where = w,
                        // git's enum callbacks fail silently here: exit 129 with
                        // no diagnostic on either stream.
                        None => return Err(GitError::Exit(129)),
                    }
                } else if let Some(value) = match_value_option(args, &mut idx, "--if-exists")? {
                    match parse_if_exists(&value) {
                        Some(v) => cur_if_exists = v,
                        None => return Err(GitError::Exit(129)),
                    }
                } else if let Some(value) = match_value_option(args, &mut idx, "--if-missing")? {
                    match parse_if_missing(&value) {
                        Some(v) => cur_if_missing = v,
                        None => return Err(GitError::Exit(129)),
                    }
                } else if arg == "--no-where" {
                    cur_where = opts.default_where;
                } else if arg == "--no-if-exists" {
                    cur_if_exists = opts.default_if_exists;
                } else if arg == "--no-if-missing" {
                    cur_if_missing = opts.default_if_missing;
                } else if arg == "--no-trailer" {
                    // `--no-trailer` clears all queued trailers in git.
                    opts.trailers.clear();
                } else if let Some(name) = arg.strip_prefix("--") {
                    return unknown_option(name, false);
                } else if arg.starts_with('-') && arg.len() > 1 {
                    // Short options other than -h are unknown to this command;
                    // git calls these "switch"es rather than "option"s.
                    return unknown_option(&arg[1..], true);
                } else {
                    opts.files.push(arg.clone());
                }
            }
        }
        idx += 1;
    }

    Ok(Invocation::Run(Box::new(opts)))
}

/// If `args[*idx]` is `name` (consuming the following argument as the value) or
/// `name=value`, return the value and advance `*idx` past any consumed value
/// argument. Returns `Ok(None)` when the current argument is not `name`.
fn match_value_option(args: &[String], idx: &mut usize, name: &str) -> Result<Option<String>> {
    let arg = &args[*idx];
    if arg == name {
        match args.get(*idx + 1) {
            Some(value) => {
                *idx += 1;
                Ok(Some(value.clone()))
            }
            None => {
                // git prints just this diagnostic (no usage block) and exits
                // 129; the option name appears without leading dashes.
                eprintln!(
                    "error: option `{}' requires a value",
                    name.trim_start_matches('-')
                );
                Err(GitError::Exit(129))
            }
        }
    } else if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
        Ok(Some(value.to_string()))
    } else {
        Ok(None)
    }
}

/// Emit git's unknown-option diagnostic and return exit 129. Long options are
/// reported as `error: unknown option \`x'`; short ones as
/// `error: unknown switch \`x'` (matching parse-options). Usage follows on
/// stderr in both cases.
fn unknown_option(name: &str, is_switch: bool) -> Result<Invocation> {
    if is_switch {
        eprintln!("error: unknown switch `{name}'");
    } else {
        eprintln!("error: unknown option `{name}'");
    }
    eprint!("{USAGE}");
    Err(GitError::Exit(129))
}

fn parse_where(value: &str) -> Option<Where> {
    match value {
        "after" => Some(Where::After),
        "before" => Some(Where::Before),
        "end" => Some(Where::End),
        "start" => Some(Where::Start),
        _ => None,
    }
}

fn parse_if_exists(value: &str) -> Option<IfExists> {
    match value {
        "addIfDifferent" => Some(IfExists::AddIfDifferent),
        "addIfDifferentNeighbor" => Some(IfExists::AddIfDifferentNeighbor),
        "add" => Some(IfExists::Add),
        "replace" => Some(IfExists::Replace),
        "doNothing" => Some(IfExists::DoNothing),
        _ => None,
    }
}

fn parse_if_missing(value: &str) -> Option<IfMissing> {
    match value {
        "doNothing" => Some(IfMissing::DoNothing),
        "add" => Some(IfMissing::Add),
        _ => None,
    }
}

/// Split a `--trailer` argument into token/value using git's `find_separator`
/// with the separator set augmented by `=` (git always accepts `=` for
/// command-line trailers). The token before the separator has trailing
/// whitespace trimmed; the value after it is whitespace-trimmed. When no valid
/// separator is found the whole argument is the token and the value is empty
/// (so `Naïve=café`, whose token byte `ï` is not a valid token character, keeps
/// the literal `Naïve=café` as its token).
fn parse_trailer_arg(
    raw: &str,
    separators: &[char],
    where_: Where,
    if_exists: IfExists,
    if_missing: IfMissing,
) -> ArgTrailer {
    // `=` plus the configured separators (deduplicated order does not matter:
    // find_separator returns the first matching byte).
    let mut arg_separators: Vec<char> = vec!['='];
    for &sep in separators {
        if sep != '=' {
            arg_separators.push(sep);
        }
    }
    let (token, value) = match find_separator(raw, &arg_separators) {
        Some(i) => {
            let token = &raw[..i];
            // The separator is a single ASCII byte for the '='/':'-class chars
            // find_separator can match.
            let rest = &raw[i + 1..];
            (token, rest)
        }
        None => (raw, ""),
    };
    ArgTrailer {
        token: token.trim_end().to_string(),
        value: value.trim().to_string(),
        where_,
        if_exists,
        if_missing,
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Defaults sourced from configuration (`trailer.*`, `core.commentChar`).
struct TrailerConfig {
    where_: Where,
    if_exists: IfExists,
    if_missing: IfMissing,
    out_separator: char,
    separators: Vec<char>,
    comment_prefix: String,
}

/// Read the relevant config keys. `-c`/`GIT_CONFIG_*` overrides take precedence,
/// then the repository config file when one is discoverable; absent everything,
/// git's compiled-in defaults apply. Reading is entirely best-effort so the
/// command still works outside a repository.
fn load_trailer_config() -> TrailerConfig {
    let mut cfg = TrailerConfig {
        where_: Where::End,
        if_exists: IfExists::AddIfDifferentNeighbor,
        if_missing: IfMissing::Add,
        out_separator: ':',
        separators: vec![':'],
        comment_prefix: "#".to_string(),
    };

    if let Some(value) = config_lookup("trailer.where") {
        if let Some(w) = parse_where(&value) {
            cfg.where_ = w;
        }
    }
    if let Some(value) = config_lookup("trailer.ifexists") {
        if let Some(v) = parse_if_exists(&value) {
            cfg.if_exists = v;
        }
    }
    if let Some(value) = config_lookup("trailer.ifmissing") {
        if let Some(v) = parse_if_missing(&value) {
            cfg.if_missing = v;
        }
    }
    if let Some(value) = config_lookup("trailer.separators") {
        if !value.is_empty() {
            cfg.separators = value.chars().collect();
            if let Some(first) = value.chars().next() {
                cfg.out_separator = first;
            }
        }
    }
    if let Some(value) = config_lookup("core.commentchar") {
        if !value.is_empty() {
            cfg.comment_prefix = value;
        }
    }

    cfg
}

/// Look up a single config value (case-insensitive key) honouring `-c`/env
/// overrides first, then the repository config file when present.
fn config_lookup(key: &str) -> Option<String> {
    if let Ok(Some(value)) = global_config_value(key) {
        return Some(value);
    }
    let git_dir = discover_git_dir(env::current_dir().ok()?).ok()?;
    let config = read_repo_config(&git_dir).ok()?;
    let (section, sub, name) = split_config_key(key)?;
    config
        .get(section, sub, name)
        .map(|value| value.to_string())
}

/// Split a dotted config key into (section, subsection, name). Only the simple
/// two-component `section.name` form is needed for the keys we read.
fn split_config_key(key: &str) -> Option<(&str, Option<&str>, &str)> {
    let dot = key.find('.')?;
    let section = &key[..dot];
    let name = &key[dot + 1..];
    if name.contains('.') {
        return None;
    }
    Some((section, None, name))
}

// ---------------------------------------------------------------------------
// Trailer model
// ---------------------------------------------------------------------------

/// One entry of a parsed trailer block. git keeps *every* non-comment line of
/// the block as an item: a line with a valid separator becomes a *token item*
/// (`token = Some(..)`), while any other line (prose, a `Key=value` line whose
/// `=` is not a recognised input separator, …) becomes a *raw item*
/// (`token = None`) whose `value` holds the line verbatim. Raw items are
/// reproduced as-is on output (and dropped under `--only-trailers`), and never
/// participate in `--trailer` matching.
///
/// For a token item, `value` is the post-separator text of the merged trailer
/// (continuation lines already folded in with their embedded newlines), trimmed
/// at both ends — exactly the strbuf git carries. Under `--unfold` that value is
/// collapsed to a single line at parse time, matching git's `unfold_value`.
#[derive(Debug, Clone)]
struct Trailer {
    /// `Some(token)` for a real trailer; `None` for a preserved raw line.
    token: Option<String>,
    /// Token value (possibly multi-line, embedded `\n`) or the verbatim raw line.
    value: String,
    /// The configured output separator at parse time, used when re-rendering.
    separator: char,
}

impl Trailer {
    /// Construct a token item.
    fn token_item(token: String, value: String, separator: char) -> Self {
        Trailer {
            token: Some(token),
            value,
            separator,
        }
    }

    /// Construct a raw (non-token) item holding `line` verbatim.
    fn raw_item(line: String) -> Self {
        Trailer {
            token: None,
            value: line,
            separator: ':',
        }
    }

    /// A token item's value is empty (used by `--trim-empty`). git's check is
    /// `!strlen(item->value)`.
    fn is_empty_value(&self) -> bool {
        self.value.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Core processing
// ---------------------------------------------------------------------------

/// Apply the whole transformation to one message and return the rendered text.
///
/// This follows git's `process_trailers` byte-offset model exactly:
///   * `end_of_log` = the end of the editable log region (start of the `---`
///     divider, minus trailing ignorable comment/blank bytes).
///   * `block_start` = the byte offset where the trailer block begins
///     (`find_trailer_block_start`); when there is no trailer block this equals
///     `end_of_log`, so the block is empty and trailers are appended there.
///   * Output = `input[0..block_start]` (verbatim body) + an optional single
///     blank line (only when one does not already precede the block) + the
///     rendered trailers + `input[end_of_log..]` (trailing blanks, divider, and
///     patch, all preserved verbatim).
fn process_message(raw_input: &str, opts: &Options) -> String {
    // git reads the whole message into a strbuf and guarantees it ends with a
    // newline before parsing; reproduce that so a file lacking a trailing
    // newline still gets one (and the body/trailer separator math lines up).
    let normalized;
    let input: &str = if raw_input.is_empty() || raw_input.ends_with('\n') {
        raw_input
    } else {
        normalized = format!("{raw_input}\n");
        &normalized
    };

    let end_of_log = find_end_of_log_message(input, opts.no_divider, &opts.comment_prefix);
    let block_start = find_trailer_block_start(input, end_of_log, opts);

    // Parse the existing trailer block [block_start, end_of_log).
    let block_text = &input[block_start..end_of_log];
    let mut trailers = parse_trailers(block_text, opts);

    // Apply queued --trailer args (unless --only-input).
    if !opts.only_input {
        for arg in &opts.trailers {
            apply_arg(&mut trailers, arg, opts.out_separator);
        }
    }

    // Note: `--trim-empty` filtering happens per item in `push_trailer`
    // (git's `format_trailers`), so empty *token* values are dropped there while
    // raw lines are preserved.

    // --only-trailers prints just the trailers, nothing else.
    if opts.only_trailers {
        let mut out = String::new();
        for trailer in &trailers {
            push_trailer(&mut out, trailer, opts);
        }
        return out;
    }

    let mut out = String::new();
    // Body verbatim.
    out.push_str(&input[..block_start]);
    // Separator blank line, unless one already ends the body region.
    if !ends_with_blank_line(&input[..block_start]) {
        out.push('\n');
    }
    // Trailers.
    for trailer in &trailers {
        push_trailer(&mut out, trailer, opts);
    }
    // Everything from end_of_log onward (trailing blanks + divider + patch).
    out.push_str(&input[end_of_log..]);
    out
}

// ---------------------------------------------------------------------------
// Line/offset primitives (mirroring trailer.c helpers)
// ---------------------------------------------------------------------------

/// git's `next_line`: byte offset just past the next `\n` at or after `pos`, or
/// the end of the buffer when there is no further newline.
fn next_line(buf: &str, pos: usize) -> usize {
    match buf.as_bytes()[pos..].iter().position(|&b| b == b'\n') {
        Some(rel) => pos + rel + 1,
        None => buf.len(),
    }
}

/// git's `last_line(buf, len)`: byte offset of the start of the last line within
/// `buf[..len]`, or `None` when the region is empty.
fn last_line(buf: &str, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if len == 1 {
        return Some(0);
    }
    let bytes = buf.as_bytes();
    let mut i = len - 2;
    loop {
        if bytes[i] == b'\n' {
            return Some(i + 1);
        }
        if i == 0 {
            return Some(0);
        }
        i -= 1;
    }
}

/// git's `is_blank_line`: the line starting at `pos` is empty or contains only
/// whitespace up to its newline.
fn is_blank_line_at(buf: &str, pos: usize) -> bool {
    for &b in &buf.as_bytes()[pos..] {
        if b == b'\n' {
            return true;
        }
        if !b.is_ascii_whitespace() {
            return false;
        }
    }
    true
}

/// True when the line starting at `pos` is *empty* (just a newline or the end of
/// input) — the stricter test git's `ignored_log_message_bytes` uses
/// (`buf[bol] == '\n'`), distinct from a whitespace-only blank line.
fn is_empty_line_at(buf: &str, pos: usize) -> bool {
    matches!(buf.as_bytes().get(pos), None | Some(b'\n'))
}

/// True when the last line of `buf` (the whole slice) is a blank line; used for
/// the body/trailer separator decision (`ends_with_blank_line`).
fn ends_with_blank_line(buf: &str) -> bool {
    match last_line(buf, buf.len()) {
        Some(ll) => is_blank_line_at(buf, ll),
        None => false,
    }
}

/// Does the line beginning at `pos` start with the comment prefix?
fn is_comment_line_at(buf: &str, pos: usize, comment_prefix: &str) -> bool {
    !comment_prefix.is_empty() && buf[pos..].starts_with(comment_prefix)
}

// ---------------------------------------------------------------------------
// Log-region boundaries
// ---------------------------------------------------------------------------

/// git's `find_end_of_log_message`: the editable log ends at the `---` divider
/// (unless `no_divider`), then trailing ignorable comment/blank bytes are
/// removed so they live with the patch tail rather than the trailer block.
fn find_end_of_log_message(input: &str, no_divider: bool, comment_prefix: &str) -> usize {
    let mut end = input.len();
    if !no_divider {
        let mut s = 0;
        while s < input.len() {
            if is_divider_at(input, s) {
                end = s;
                break;
            }
            s = next_line(input, s);
        }
    }
    end - ignored_log_message_bytes(input, end, comment_prefix)
}

/// A divider line begins at `pos` if it is `---` followed by whitespace (or end
/// of line): git's `skip_prefix(s, "---", &v) && isspace(*v)`.
fn is_divider_at(input: &str, pos: usize) -> bool {
    let rest = &input[pos..];
    let Some(after) = rest.strip_prefix("---") else {
        return false;
    };
    match after.as_bytes().first() {
        None => true,
        Some(&b) => b.is_ascii_whitespace(),
    }
}

/// git's `ignored_log_message_bytes`: count the trailing run of *empty* lines
/// and comment lines (also tolerating an old-style `Conflicts:` block) at the
/// end of `buf[..len]`. These bytes are treated as belonging to the patch tail.
///
/// Faithful to a C subtlety: `boc` ("beginning of comments") is a `size_t`
/// initialised to 0, and the return is `boc ? len - boc : len - cutoff`. A run
/// that begins at offset 0 therefore makes `boc` *falsy*, so git returns
/// `len - cutoff` (zero, with no scissors) — i.e. a leading comment/blank that
/// spans the whole region is **not** trimmed and stays in the body. We model
/// that by treating `boc == 0` exactly like "no run".
fn ignored_log_message_bytes(buf: &str, len: usize, comment_prefix: &str) -> usize {
    // We do not implement scissors detection (`wt_status_locate_end`); the
    // common path has no scissors line, so the cutoff is `len`. A scissors line
    // is itself a comment and so is absorbed into the trailing run anyway.
    let cutoff = len;
    let mut boc = 0usize;
    let mut boc_set = false;
    let mut in_conflicts = false;
    let mut bol = 0;
    while bol < cutoff {
        let nl = next_line(buf, bol);
        if is_comment_line_at(buf, bol, comment_prefix) || is_empty_line_at(buf, bol) {
            if !boc_set {
                boc = bol;
                boc_set = true;
            }
        } else if buf[bol..].starts_with("Conflicts:\n") {
            in_conflicts = true;
            if !boc_set {
                boc = bol;
                boc_set = true;
            }
        } else if in_conflicts && buf.as_bytes().get(bol) == Some(&b'\t') {
            // a pathname inside the conflicts block — keep scanning
        } else if boc_set {
            boc = 0;
            boc_set = false;
            in_conflicts = false;
        }
        bol = nl;
    }
    // `boc ? len - boc : len - cutoff` — note boc == 0 is the falsy branch.
    if boc != 0 {
        len - boc
    } else {
        len - cutoff
    }
}

// ---------------------------------------------------------------------------
// Trailer-block detection
// ---------------------------------------------------------------------------

/// git's `find_trailer_block_start`: scan backward over the final paragraph of
/// `buf[..len]` and return the byte offset where the trailer block begins. When
/// no trailer block is found (including the whole message being the title) this
/// returns `len`, i.e. an empty block at the end of the log region.
fn find_trailer_block_start(buf: &str, len: usize, opts: &Options) -> usize {
    // The first paragraph is the title and cannot be trailers: advance over it
    // (skipping comment lines) to the first blank line.
    let mut s = 0;
    while s < len {
        if is_comment_line_at(buf, s, &opts.comment_prefix) {
            s = next_line(buf, s);
            continue;
        }
        if is_blank_line_at(buf, s) {
            break;
        }
        s = next_line(buf, s);
    }
    let end_of_title = s;

    let mut only_spaces = true;
    let mut recognized_prefix = false;
    let mut trailer_lines = 0i64;
    let mut non_trailer_lines = 0i64;
    let mut possible_continuation = 0i64;

    let mut maybe_l = last_line(buf, len);
    while let Some(l) = maybe_l {
        if l < end_of_title {
            break;
        }
        let bol = l;

        if is_comment_line_at(buf, bol, &opts.comment_prefix) {
            non_trailer_lines += possible_continuation;
            possible_continuation = 0;
        } else if is_blank_line_at(buf, bol) {
            if only_spaces {
                // Skip a trailing blank line and keep scanning upward.
            } else {
                non_trailer_lines += possible_continuation;
                if recognized_prefix && trailer_lines * 3 >= non_trailer_lines {
                    return next_line(buf, bol);
                } else if trailer_lines > 0 && non_trailer_lines == 0 {
                    return next_line(buf, bol);
                }
                return len;
            }
        } else {
            only_spaces = false;
            let first_byte = buf.as_bytes()[bol];
            if buf[bol..].starts_with("Signed-off-by: ")
                || buf[bol..].starts_with("(cherry picked from commit ")
            {
                trailer_lines += 1;
                possible_continuation = 0;
                recognized_prefix = true;
            } else if let Some(sep) = separator_index(line_at(buf, bol, len), &opts.separators) {
                let _ = sep;
                trailer_lines += 1;
                possible_continuation = 0;
                // (git additionally marks recognized_prefix when the token
                // matches a configured trailer.<key>; we do not carry per-key
                // config, so this only affects the obscure 25%-with-config path.)
            } else if first_byte.is_ascii_whitespace() {
                possible_continuation += 1;
            } else {
                non_trailer_lines += 1;
                non_trailer_lines += possible_continuation;
                possible_continuation = 0;
            }
        }

        // Move to the previous line (last_line of the region before `l`).
        if l == 0 {
            break;
        }
        maybe_l = last_line(buf, l);
    }

    len
}

/// The text of the line beginning at `pos`, bounded by `len`, with its trailing
/// newline (if any) excluded — what `separator_index` expects.
fn line_at(buf: &str, pos: usize, len: usize) -> &str {
    let end = match buf.as_bytes()[pos..len].iter().position(|&b| b == b'\n') {
        Some(rel) => pos + rel,
        None => len,
    };
    &buf[pos..end]
}

/// Faithful port of git's `find_separator`: return the byte index of the first
/// separator character in `line`, or `None`. The token preceding the separator
/// may consist only of ASCII alphanumerics and `-`, optionally followed by
/// trailing spaces/tabs before the separator. Any other character (including a
/// non-ASCII byte) ends the scan with no separator.
///
/// git callers require the result to be `>= 1`; this helper returns the raw
/// position (which can be 0 for a leading separator) and leaves that check to
/// [`is_trailer_line`]. Operates on bytes to match C's byte-wise `isalnum`.
fn find_separator(line: &str, separators: &[char]) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut whitespace_found = false;
    for (i, &b) in bytes.iter().enumerate() {
        let ch = b as char;
        if separators.contains(&ch) {
            return Some(i);
        }
        if !whitespace_found && (b.is_ascii_alphanumeric() || b == b'-') {
            continue;
        }
        if i != 0 && (b == b' ' || b == b'\t') {
            whitespace_found = true;
            continue;
        }
        break;
    }
    None
}

/// A line is a trailer when `find_separator` yields a position `>= 1` and the
/// line does not begin with whitespace (git's
/// `separator_pos >= 1 && !isspace(bol[0])`). Returns the separator byte index.
fn separator_index(line: &str, separators: &[char]) -> Option<usize> {
    if line
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_whitespace())
    {
        return None;
    }
    match find_separator(line, separators) {
        Some(pos) if pos >= 1 => Some(pos),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Trailer parsing
// ---------------------------------------------------------------------------

/// A raw line of the trailer block after continuation-merging, mirroring git's
/// `trailer_block_get`: the first physical line plus any continuation lines that
/// attached to it. `has_separator` records whether the *first* physical line had
/// a valid separator (which is what git's `last` anchor tracks).
struct RawTrailerLine {
    /// The first physical line (newline stripped).
    head: String,
    /// Continuation lines (leading whitespace preserved, newline stripped).
    continuation: Vec<String>,
    has_separator: bool,
}

/// Phase 1 (`trailer_block_get`): split the block into logical lines, attaching
/// a leading-whitespace line to the previous logical line only when that line
/// had a separator (git's `if (last && isspace(buf[0]))`).
fn split_block_lines(block: &str, separators: &[char]) -> Vec<RawTrailerLine> {
    let mut lines: Vec<RawTrailerLine> = Vec::new();
    let mut last_has_sep = false;
    for raw in block.split_inclusive('\n') {
        let text = raw.strip_suffix('\n').unwrap_or(raw);
        if text.starts_with([' ', '\t']) && last_has_sep {
            if let Some(last) = lines.last_mut() {
                last.continuation.push(text.to_string());
                continue;
            }
        }
        let has_separator = find_separator(text, separators).is_some_and(|p| p >= 1);
        lines.push(RawTrailerLine {
            head: text.to_string(),
            continuation: Vec::new(),
            has_separator,
        });
        last_has_sep = has_separator;
    }
    lines
}

/// Phase 2 (`parse_trailers`): turn the merged logical lines into structured
/// [`Trailer`]s. Comment lines are dropped outright; a line with a valid
/// separator becomes a token item; any other line becomes a raw item — except
/// under `--only-trailers`, where non-token lines are dropped at parse time too.
fn parse_trailers(block: &str, opts: &Options) -> Vec<Trailer> {
    let mut trailers: Vec<Trailer> = Vec::new();
    for line in split_block_lines(block, &opts.separators) {
        // git: `if (starts_with(trailer, comment_line_str)) continue;`
        if !opts.comment_prefix.is_empty() && line.head.starts_with(&opts.comment_prefix) {
            continue;
        }
        match separator_index(&line.head, &opts.separators) {
            Some(sep) => {
                let token = line.head[..sep].trim_end().to_string();
                // git's `parse_trailer` takes the post-separator text of the
                // whole merged trailer (continuation lines included, joined by
                // the original newlines) and trims it once.
                let mut merged = line.head[sep + 1..].to_string();
                for cont in &line.continuation {
                    merged.push('\n');
                    merged.push_str(cont);
                }
                let mut value = merged.trim().to_string();
                if opts.unfold {
                    value = unfold_value(&value);
                }
                trailers.push(Trailer::token_item(token, value, opts.out_separator));
            }
            None => {
                if !opts.only_trailers {
                    trailers.push(Trailer::raw_item(line.head));
                }
            }
        }
    }
    trailers
}

/// Faithful port of git's `unfold_value`: each newline plus the whitespace run
/// that follows it collapses to a single space; all other characters (including
/// spaces not preceded by a newline) are preserved; the result is trimmed.
/// Iterates over `char`s so multibyte UTF-8 values survive intact.
fn unfold_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            while chars.peek().is_some_and(|n| n.is_whitespace()) {
                chars.next();
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Applying --trailer arguments
// ---------------------------------------------------------------------------

/// Whether the placement inserts *after* the reference / at the *end*
/// (git's `after_or_end`).
fn after_or_end(where_: Where) -> bool {
    matches!(where_, Where::After | Where::End)
}

/// git's `same_token`: case-insensitive comparison over the shorter of the two
/// tokens, so a prefix like `Ack` matches `Acked-by`. Raw items (no token) never
/// match (`if (!a->token) return 0`).
fn item_same_token(item: &Trailer, arg_token: &str) -> bool {
    match &item.token {
        Some(tok) => same_token(tok, arg_token),
        None => false,
    }
}

/// Token comparison over the shorter length (git's `same_token` core).
fn same_token(a: &str, b: &str) -> bool {
    let min_len = a.len().min(b.len());
    a.as_bytes()[..min_len].eq_ignore_ascii_case(&b.as_bytes()[..min_len])
}

/// git's `same_value`: case-insensitive value comparison.
fn same_value(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn same_trailer(item: &Trailer, arg: &ArgTrailer) -> bool {
    item_same_token(item, &arg.token) && same_value(&item.value, &arg.value)
}

/// Apply a single queued trailer to the list, honouring its where/if-exists/
/// if-missing policy. Mirrors `find_same_and_apply_arg` / `apply_arg_if_exists`
/// / `add_arg_to_input_list` from trailer.c, including the `on_tok` reference
/// (the list head/tail for start/end, the matched item for after/before) and the
/// neighbor-vs-whole-list distinction between the two `addIfDifferent` modes.
fn apply_arg(trailers: &mut Vec<Trailer>, arg: &ArgTrailer, out_sep: char) {
    let new = Trailer::token_item(arg.token.clone(), arg.value.clone(), out_sep);
    let backwards = after_or_end(arg.where_);
    let middle = matches!(arg.where_, Where::After | Where::Before);

    // find_same_and_apply_arg: locate the first same-token item in the search
    // direction. `start_idx` is the tail (backwards) or head (forwards).
    if trailers.is_empty() {
        // No existing trailers at all => if-missing applies; insertion falls back
        // to start/end since there is no reference item.
        if matches!(arg.if_missing, IfMissing::Add) {
            insert_relative(trailers, new, None, backwards);
        }
        return;
    }
    let start_idx = if backwards { trailers.len() - 1 } else { 0 };
    let match_idx = find_same_token(trailers, &arg.token, backwards);

    let Some(in_idx) = match_idx else {
        // if-missing path: no same-token trailer exists.
        if matches!(arg.if_missing, IfMissing::Add) {
            // on_tok is start_tok (head/tail); insert relative to it.
            insert_relative(trailers, new, Some(start_idx), backwards);
        }
        return;
    };

    // on_tok index: the matched item for after/before, else start_tok.
    let on_idx = if middle { in_idx } else { start_idx };

    match arg.if_exists {
        IfExists::DoNothing => {}
        IfExists::Replace => {
            // git: add the new item relative to on_tok, then delete in_tok (the
            // single matched item). Mirror the insert-then-delete order, fixing
            // up in_tok's index for the shift the insertion may have caused.
            let inserted_at = insert_relative(trailers, new, Some(on_idx), backwards);
            let in_after = if inserted_at <= in_idx {
                in_idx + 1
            } else {
                in_idx
            };
            trailers.remove(in_after);
        }
        IfExists::Add => {
            insert_relative(trailers, new, Some(on_idx), backwards);
        }
        IfExists::AddIfDifferent => {
            // Compare against the whole list, starting at in_tok, in direction.
            if check_if_different(trailers, in_idx, arg, true, backwards) {
                insert_relative(trailers, new, Some(on_idx), backwards);
            }
        }
        IfExists::AddIfDifferentNeighbor => {
            // Compare only the immediate neighbor: start at on_tok, one step.
            if check_if_different(trailers, on_idx, arg, false, backwards) {
                insert_relative(trailers, new, Some(on_idx), backwards);
            }
        }
    }
}

/// Find the index of the first same-token trailer scanning in `backwards`
/// direction (from the tail when backwards, else from the head). Raw items never
/// match.
fn find_same_token(trailers: &[Trailer], token: &str, backwards: bool) -> Option<usize> {
    if backwards {
        (0..trailers.len())
            .rev()
            .find(|&i| item_same_token(&trailers[i], token))
    } else {
        (0..trailers.len()).find(|&i| item_same_token(&trailers[i], token))
    }
}

/// git's `check_if_different`: starting at `in_tok` (index `from`), walk in the
/// insertion direction (prev for after/end, next for before/start) comparing the
/// full trailer; return false (not different) on a match. With `check_all=false`
/// only the starting item is compared.
fn check_if_different(
    trailers: &[Trailer],
    from: usize,
    arg: &ArgTrailer,
    check_all: bool,
    backwards: bool,
) -> bool {
    let mut idx = from as isize;
    loop {
        if idx < 0 || idx as usize >= trailers.len() {
            break;
        }
        let i = idx as usize;
        if same_trailer(&trailers[i], arg) {
            return false;
        }
        if !check_all {
            break;
        }
        // Move toward the head boundary in the insertion direction.
        idx += if backwards { -1 } else { 1 };
    }
    true
}

/// Insert `new` relative to a reference index, reproducing
/// `add_arg_to_input_list`: for after/end insert *after* the reference; for
/// before/start insert *before* it. When `reference` is `None`, insert at the
/// end (after/end) or start (before/start). Returns the index where `new`
/// landed.
fn insert_relative(
    trailers: &mut Vec<Trailer>,
    new: Trailer,
    reference: Option<usize>,
    backwards: bool,
) -> usize {
    match reference {
        Some(ref_idx) => {
            if backwards {
                // insert after the reference
                let at = ref_idx + 1;
                trailers.insert(at, new);
                at
            } else {
                // insert before the reference
                trailers.insert(ref_idx, new);
                ref_idx
            }
        }
        None => {
            if backwards {
                trailers.push(new);
                trailers.len() - 1
            } else {
                trailers.insert(0, new);
                0
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Append one trailer item to `out`, mirroring git's per-item logic in
/// `format_trailers`. Honours `--only-trailers` and `--trim-empty`; `--unfold`
/// has already been applied to token values at parse time.
///
///   * Raw items (no token) are reproduced verbatim, but only when not
///     `--only-trailers`.
///   * Token items with an empty value are skipped under `--trim-empty`.
///   * A token item prints `token<sep> value`, where the separator is appended
///     only when the token does not already end in one (git's
///     `last_non_space_char` check); an empty value still yields the trailing
///     `<sep> ` (e.g. `Acked-by: `). A multi-line value carries its embedded
///     newlines (continuation lines) verbatim.
fn push_trailer(out: &mut String, trailer: &Trailer, opts: &Options) {
    let Some(token) = &trailer.token else {
        // Raw (non-token) line: keep verbatim unless only printing trailers.
        if !opts.only_trailers {
            out.push_str(&trailer.value);
            out.push('\n');
        }
        return;
    };

    if opts.trim_empty && trailer.is_empty_value() {
        return;
    }

    out.push_str(token);
    // Separator: append `<sep> ` unless the token already ends with a separator
    // character (ignoring trailing spaces).
    let needs_sep = last_non_space_char(token)
        .is_none_or(|c| c != trailer.separator && !opts.separators.contains(&c));
    if needs_sep {
        out.push(trailer.separator);
        out.push(' ');
    }

    out.push_str(&trailer.value);
    out.push('\n');
}

/// The last non-space character of `s`, or `None` when `s` is empty or all
/// spaces (git's `last_non_space_char`).
fn last_non_space_char(s: &str) -> Option<char> {
    s.chars().rev().find(|c| !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_opts() -> Options {
        Options {
            in_place: false,
            trim_empty: false,
            only_trailers: false,
            only_input: false,
            unfold: false,
            no_divider: false,
            default_where: Where::End,
            default_if_exists: IfExists::AddIfDifferentNeighbor,
            default_if_missing: IfMissing::Add,
            out_separator: ':',
            separators: vec![':'],
            comment_prefix: "#".to_string(),
            trailers: Vec::new(),
            files: Vec::new(),
        }
    }

    fn with_trailers(specs: &[(&str, &str)]) -> Options {
        let mut opts = default_opts();
        for (token, value) in specs {
            opts.trailers.push(ArgTrailer {
                token: (*token).to_string(),
                value: (*value).to_string(),
                where_: Where::End,
                if_exists: IfExists::AddIfDifferentNeighbor,
                if_missing: IfMissing::Add,
            });
        }
        opts
    }

    #[test]
    fn divider_detection() {
        assert!(is_divider_at("---", 0));
        assert!(is_divider_at("--- ", 0));
        assert!(is_divider_at("--- foo", 0));
        assert!(!is_divider_at("----", 0));
        assert!(!is_divider_at("---x", 0));
        assert!(!is_divider_at("--", 0));
    }

    #[test]
    fn separator_validation() {
        let seps = vec![':'];
        assert_eq!(separator_index("Key: v", &seps), Some(3));
        assert_eq!(separator_index(":v", &seps), None); // empty token
        assert_eq!(separator_index(" Key: v", &seps), None); // leading ws
        assert_eq!(separator_index("See http://x", &seps), None); // token has ws
        assert_eq!(separator_index("plain text", &seps), None);
    }

    #[test]
    fn add_to_existing_block() {
        let out = process_message(
            "subj\n\nbody\n\nSigned-off-by: A <a@x>\n",
            &with_trailers(&[("Acked-by", "B <b@x>")]),
        );
        assert_eq!(
            out,
            "subj\n\nbody\n\nSigned-off-by: A <a@x>\nAcked-by: B <b@x>\n"
        );
    }

    #[test]
    fn add_creates_block_after_body() {
        let out = process_message("subj\n\nbody\n", &with_trailers(&[("Sob", "X")]));
        assert_eq!(out, "subj\n\nbody\n\nSob: X\n");
    }

    #[test]
    fn subject_only_gets_blank_separator() {
        let out = process_message("subj\n", &with_trailers(&[("Sob", "X")]));
        assert_eq!(out, "subj\n\nSob: X\n");
    }

    #[test]
    fn single_paragraph_is_not_trailers() {
        // A lone trailer-looking paragraph is the message body, so the new
        // trailer starts a fresh paragraph.
        let out = process_message("Ack: 1\nRev: 2\n", &with_trailers(&[("New", "x")]));
        assert_eq!(out, "Ack: 1\nRev: 2\n\nNew: x\n");
    }

    #[test]
    fn only_trailers_filters_body() {
        let mut opts = default_opts();
        opts.only_trailers = true;
        let out = process_message("subj\n\nbody\n\nAck: 1\nRev: 2\n", &opts);
        assert_eq!(out, "Ack: 1\nRev: 2\n");
    }

    #[test]
    fn trim_empty_drops_empty_values() {
        let mut opts = default_opts();
        opts.trim_empty = true;
        opts.only_trailers = true;
        let out = process_message("subj\n\nAck:\nRev: 2\n", &opts);
        assert_eq!(out, "Rev: 2\n");
    }

    #[test]
    fn unfold_joins_continuations() {
        let mut opts = default_opts();
        opts.only_trailers = true;
        opts.unfold = true;
        let out = process_message("subj\n\nAck: a\n  b\n\tc\n", &opts);
        assert_eq!(out, "Ack: a b c\n");
    }

    #[test]
    fn if_exists_replace_swaps_matched_only() {
        // git's replace removes only the single matched trailer (the last one in
        // the default end/backwards search) and appends the replacement, leaving
        // the earlier same-token trailer untouched.
        let mut opts = with_trailers(&[("Acked-by", "D")]);
        opts.trailers[0].if_exists = IfExists::Replace;
        let out = process_message("subj\n\nbody\n\nAcked-by: B\nAcked-by: C\n", &opts);
        assert_eq!(out, "subj\n\nbody\n\nAcked-by: B\nAcked-by: D\n");
    }

    #[test]
    fn if_exists_do_nothing() {
        let mut opts = with_trailers(&[("Acked-by", "C")]);
        opts.trailers[0].if_exists = IfExists::DoNothing;
        let out = process_message("subj\n\nbody\n\nAcked-by: B\n", &opts);
        assert_eq!(out, "subj\n\nbody\n\nAcked-by: B\n");
    }

    #[test]
    fn if_missing_do_nothing() {
        let mut opts = with_trailers(&[("Reviewed-by", "C")]);
        opts.trailers[0].if_missing = IfMissing::DoNothing;
        let out = process_message("subj\n\nbody\n\nAcked-by: B\n", &opts);
        assert_eq!(out, "subj\n\nbody\n\nAcked-by: B\n");
    }

    #[test]
    fn default_neighbor_dedup() {
        // Same value as the last trailer of the same key => not added.
        let out = process_message(
            "subj\n\nbody\n\nB: 2\nA: 1\n",
            &with_trailers(&[("A", "1")]),
        );
        assert_eq!(out, "subj\n\nbody\n\nB: 2\nA: 1\n");
    }

    #[test]
    fn neighbor_different_is_added() {
        // Last trailer overall (B:2) differs from A:1 => added at end.
        let out = process_message(
            "subj\n\nbody\n\nA: 1\nB: 2\n",
            &with_trailers(&[("A", "1")]),
        );
        assert_eq!(out, "subj\n\nbody\n\nA: 1\nB: 2\nA: 1\n");
    }

    #[test]
    fn where_after_inserts_next_to_match() {
        let mut opts = with_trailers(&[("Acked-by", "NEW")]);
        opts.trailers[0].where_ = Where::After;
        let out = process_message("subj\n\nbody\n\nAcked-by: B\nReviewed-by: X\n", &opts);
        assert_eq!(
            out,
            "subj\n\nbody\n\nAcked-by: B\nAcked-by: NEW\nReviewed-by: X\n"
        );
    }

    #[test]
    fn where_before_inserts_before_match() {
        let mut opts = with_trailers(&[("Acked-by", "NEW")]);
        opts.trailers[0].where_ = Where::Before;
        let out = process_message("subj\n\nbody\n\nAcked-by: B\nReviewed-by: X\n", &opts);
        assert_eq!(
            out,
            "subj\n\nbody\n\nAcked-by: NEW\nAcked-by: B\nReviewed-by: X\n"
        );
    }

    #[test]
    fn divider_preserves_patch() {
        let out = process_message(
            "subj\n\nbody\n\nA: 1\n---\ndiff stuff\nmore: x\n",
            &with_trailers(&[("B", "2")]),
        );
        assert_eq!(
            out,
            "subj\n\nbody\n\nA: 1\nB: 2\n---\ndiff stuff\nmore: x\n"
        );
    }

    #[test]
    fn no_divider_keeps_dashes_as_body() {
        let mut opts = with_trailers(&[("B", "2")]);
        opts.no_divider = true;
        let out = process_message("subj\n\nbody\n---\nmore\n", &opts);
        assert_eq!(out, "subj\n\nbody\n---\nmore\n\nB: 2\n");
    }

    #[test]
    fn trailing_blank_lines_preserved() {
        let out = process_message("subj\n\nbody\n\n\n", &with_trailers(&[("A", "1")]));
        assert_eq!(out, "subj\n\nbody\n\nA: 1\n\n\n");
    }

    #[test]
    fn arg_separator_first_of_either() {
        let seps = vec![':'];
        let t = parse_trailer_arg(
            "key=a:b",
            &seps,
            Where::End,
            IfExists::AddIfDifferentNeighbor,
            IfMissing::Add,
        );
        assert_eq!(t.token, "key");
        assert_eq!(t.value, "a:b");

        let t2 = parse_trailer_arg(
            "key:a=b",
            &seps,
            Where::End,
            IfExists::AddIfDifferentNeighbor,
            IfMissing::Add,
        );
        assert_eq!(t2.token, "key");
        assert_eq!(t2.value, "a=b");

        let t3 = parse_trailer_arg(
            "keyonly",
            &seps,
            Where::End,
            IfExists::AddIfDifferentNeighbor,
            IfMissing::Add,
        );
        assert_eq!(t3.token, "keyonly");
        assert_eq!(t3.value, "");
    }

    #[test]
    fn recognized_prefix_enables_quarter_rule() {
        let mut opts = default_opts();
        opts.only_trailers = true;
        // 1 S-o-b + 3 prose: 1*3 >= 3 => block accepted.
        let out = process_message("subj\n\nSigned-off-by: A\np1\np2\np3\n", &opts);
        assert_eq!(out, "Signed-off-by: A\n");
        // 1 S-o-b + 4 prose: 1*3 >= 4 false => no block.
        let out2 = process_message("subj\n\nSigned-off-by: A\np1\np2\np3\np4\n", &opts);
        assert_eq!(out2, "");
    }

    #[test]
    fn non_trailer_line_kills_block_without_prefix() {
        let mut opts = default_opts();
        opts.only_trailers = true;
        let out = process_message("subj\n\nA: 1\nplain line\n", &opts);
        assert_eq!(out, "");
    }
}
