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

// Glob the crate root for shared plumbing (read_repo_config,
// global_config_value, GitError, Result, io, fs, env, the Read/Write traits,
// Path/PathBuf, etc.); see commands::stash for why this is a wildcard.
use crate::*;
use sley_mail::trailers::{
    ConfItem, IfExists, IfMissing, TrailerOptions, Where, parse_trailer_arg, process_message,
};

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

/// Fully parsed command-line options: the trailer engine configuration plus
/// the CLI-only file/in-place handling.
#[derive(Debug)]
struct Options {
    in_place: bool,
    files: Vec<String>,
    /// The `sley_mail::trailers` engine configuration (separators, defaults,
    /// per-token conf items, queued `--trailer` args).
    engine: TrailerOptions,
}

/// Outcome of argument parsing: either run with options, or print help.
enum Invocation {
    Run(Box<Options>),
    Help,
}

/// Entry point for `git interpret-trailers`.
pub(crate) fn cmd_interpret_trailers(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let config = cli_session
        .git_dir()
        .ok()
        .and_then(|git_dir| read_repo_config(&git_dir).ok());
    let options = match parse_args(args, config.as_ref())? {
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
    if options.engine.only_input && !options.engine.trailers.is_empty() {
        eprintln!("fatal: --trailer with --only-input does not make sense");
        eprintln!();
        eprint!("{USAGE}");
        return Err(GitError::Exit(129));
    }

    if options.files.is_empty() {
        // git: `--in-place` with no file operands is a hard error (there is
        // nothing to edit in place) — `die("no input file given for in-place
        // editing")`, exit 128.
        if options.in_place {
            eprintln!("fatal: no input file given for in-place editing");
            return Err(GitError::Exit(128));
        }
        // No file operands: read the single message from stdin and stream the
        // result to stdout.
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        let text = String::from_utf8_lossy(&input).into_owned();
        let rendered = process_message(&text, &options.engine);
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
        let rendered = process_message(&text, &options.engine);
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

/// Apply a list of raw `--trailer <arg>` strings to a commit/tag message,
/// returning the rewritten message. This is the same engine `git commit
/// --trailer` / `git tag --trailer` use internally (`process_trailers` with the
/// command's options), so per-token `trailer.*` configuration (key/where/
/// ifexists/ifmissing/command/cmd), separators, and comment-char all apply
/// exactly as in `git interpret-trailers`.
///
/// `trailer_args` are the raw argument strings as given on the command line
/// (e.g. `"Acked-by: x"`, `"ack = Peff"`); each is split + resolved against
/// config just like `--trailer`. The message is processed as a single input;
/// configured command trailers (`trailer.<name>.command`) are also run.
///
/// Matching git's `amend_strbuf_with_trailers`, divider handling is disabled
/// (`no_divider = true`): a `---` line in a commit/tag *body* is ordinary text,
/// not a patch divider, so trailers append after it rather than before it.
/// True when `message` ends with a recognised trailer block containing at least
/// one trailer (git's `has_conforming_footer` via `trailer_iterator`). Honouring
/// `trailer.<name>.*` config is essential: a configured token can tip the 25%
/// rule so a mixed paragraph still counts as a trailer block (t7501 signoff /
/// `trailer.Myfooter.ifexists=add`).
pub(crate) fn message_has_conforming_trailer_block(
    config: Option<&GitConfig>,
    message: &str,
) -> bool {
    let mut engine = load_trailer_config(config);
    engine.only_input = true;
    engine.no_divider = true;
    sley_mail::trailers::message_has_conforming_trailer_block(message, &engine)
}

pub(crate) fn apply_trailers_to_message(
    config: Option<&GitConfig>,
    message: &str,
    trailer_args: &[String],
) -> String {
    let mut engine = load_trailer_config(config);
    // git's amend path sets `no_divider = 1`: in a commit/tag message a `---`
    // line is body text, not a patch divider.
    engine.no_divider = true;
    engine.trailers = trailer_args
        .iter()
        .map(|raw| {
            parse_trailer_arg(
                raw,
                &engine.separators,
                &engine.conf_items,
                engine.default_where,
                engine.default_if_exists,
                engine.default_if_missing,
                None,
                None,
                None,
            )
        })
        .collect();
    process_message(message, &engine)
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
fn parse_args(args: &[String], config: Option<&GitConfig>) -> Result<Invocation> {
    // Seed defaults from configuration (best-effort) before applying argv.
    let engine = load_trailer_config(config);

    let mut opts = Options {
        engine,
        in_place: false,
        files: Vec::new(),
    };

    // Each queued `--trailer` captures the placement/policy in force *at the
    // time it appears*, so a later `--where`/`--if-exists`/`--if-missing` only
    // affects subsequent trailers. git models a command-line override as a
    // `*_DEFAULT` sentinel that is replaced when explicitly set; we use `Option`
    // (None = "no command-line override in force", so the matched config item or
    // global default decides). `--no-where` etc. reset to None.
    let mut cur_where: Option<Where> = None;
    let mut cur_if_exists: Option<IfExists> = None;
    let mut cur_if_missing: Option<IfMissing> = None;

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
            "--trim-empty" => opts.engine.trim_empty = true,
            "--no-trim-empty" => opts.engine.trim_empty = false,
            "--only-trailers" => opts.engine.only_trailers = true,
            "--no-only-trailers" => opts.engine.only_trailers = false,
            "--only-input" => opts.engine.only_input = true,
            "--no-only-input" => opts.engine.only_input = false,
            "--unfold" => opts.engine.unfold = true,
            "--no-unfold" => opts.engine.unfold = false,
            "--no-divider" => opts.engine.no_divider = true,
            "--divider" => opts.engine.no_divider = false,
            "--parse" => {
                // Documented alias for --only-trailers --only-input --unfold.
                opts.engine.only_trailers = true;
                opts.engine.only_input = true;
                opts.engine.unfold = true;
            }
            _ => {
                // Value-bearing options, in both `--opt value` and `--opt=value`
                // spellings.
                if let Some(value) = match_value_option(args, &mut idx, "--trailer")? {
                    let trailer = parse_trailer_arg(
                        &value,
                        &opts.engine.separators,
                        &opts.engine.conf_items,
                        opts.engine.default_where,
                        opts.engine.default_if_exists,
                        opts.engine.default_if_missing,
                        cur_where,
                        cur_if_exists,
                        cur_if_missing,
                    );
                    opts.engine.trailers.push(trailer);
                } else if let Some(value) = match_value_option(args, &mut idx, "--where")? {
                    match parse_where(&value) {
                        Some(w) => cur_where = Some(w),
                        // git's enum callbacks fail silently here: exit 129 with
                        // no diagnostic on either stream.
                        None => return Err(GitError::Exit(129)),
                    }
                } else if let Some(value) = match_value_option(args, &mut idx, "--if-exists")? {
                    match parse_if_exists(&value) {
                        Some(v) => cur_if_exists = Some(v),
                        None => return Err(GitError::Exit(129)),
                    }
                } else if let Some(value) = match_value_option(args, &mut idx, "--if-missing")? {
                    match parse_if_missing(&value) {
                        Some(v) => cur_if_missing = Some(v),
                        None => return Err(GitError::Exit(129)),
                    }
                } else if arg == "--no-where" {
                    cur_where = None;
                } else if arg == "--no-if-exists" {
                    cur_if_exists = None;
                } else if arg == "--no-if-missing" {
                    cur_if_missing = None;
                } else if arg == "--no-trailer" {
                    // `--no-trailer` clears all queued trailers in git.
                    opts.engine.trailers.clear();
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

/// Parse a placement value. git's `trailer_set_where` compares with `strcasecmp`,
/// so the value is matched case-insensitively (`AFTER`, `Before`, … all work).
fn parse_where(value: &str) -> Option<Where> {
    match value.to_ascii_lowercase().as_str() {
        "after" => Some(Where::After),
        "before" => Some(Where::Before),
        "end" => Some(Where::End),
        "start" => Some(Where::Start),
        _ => None,
    }
}

/// Parse an if-exists value (git's `trailer_set_if_exists`, case-insensitive).
fn parse_if_exists(value: &str) -> Option<IfExists> {
    match value.to_ascii_lowercase().as_str() {
        "addifdifferent" => Some(IfExists::AddIfDifferent),
        "addifdifferentneighbor" => Some(IfExists::AddIfDifferentNeighbor),
        "add" => Some(IfExists::Add),
        "replace" => Some(IfExists::Replace),
        "donothing" => Some(IfExists::DoNothing),
        _ => None,
    }
}

/// Parse an if-missing value (git's `trailer_set_if_missing`, case-insensitive).
fn parse_if_missing(value: &str) -> Option<IfMissing> {
    match value.to_ascii_lowercase().as_str() {
        "donothing" => Some(IfMissing::DoNothing),
        "add" => Some(IfMissing::Add),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Read the relevant config keys into a [`TrailerOptions`] seed. The effective
/// `GitConfig` (which already layers `-c`/`GIT_CONFIG_*` overrides on top of the
/// repository config file) is read once and scanned: the bare
/// `trailer.where/ifexists/ifmissing/separators` keys set the global defaults,
/// `core.commentChar` sets the comment prefix, and every `trailer.<name>.<var>`
/// populates a per-token [`ConfItem`]. Reading is entirely best-effort so the
/// command still works outside a repository (git's compiled-in defaults then
/// apply).
fn load_trailer_config(config: Option<&GitConfig>) -> TrailerOptions {
    let mut cfg = TrailerOptions {
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
        conf_items: Vec::new(),
        trailers: Vec::new(),
    };

    let Some(config) = config else {
        return cfg;
    };

    // Global defaults (bare `trailer.<var>`, no subsection).
    if let Some(value) = config.get("trailer", None, "where")
        && let Some(w) = parse_where(value)
    {
        cfg.default_where = w;
    }
    if let Some(value) = config.get("trailer", None, "ifexists")
        && let Some(v) = parse_if_exists(value)
    {
        cfg.default_if_exists = v;
    }
    if let Some(value) = config.get("trailer", None, "ifmissing")
        && let Some(v) = parse_if_missing(value)
    {
        cfg.default_if_missing = v;
    }
    if let Some(value) = config.get("trailer", None, "separators")
        && !value.is_empty()
    {
        cfg.separators = value.chars().collect();
        if let Some(first) = value.chars().next() {
            cfg.out_separator = first;
        }
    }
    if let Some(value) = config.get("core", None, "commentchar")
        && !value.is_empty()
    {
        cfg.comment_prefix = value.to_string();
    }

    // Per-token items: every `trailer.<name>.<var>` subsection. We collect them in
    // config order (first appearance of each `<name>` fixes its slot), mirroring
    // git's `get_conf_item` which appends a new item the first time a name is
    // seen and updates it in place thereafter (last value wins per var).
    let mut items: Vec<ConfItem> = Vec::new();
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("trailer") {
            continue;
        }
        let Some(sub) = &section.subsection else {
            continue;
        };
        let idx = match items.iter().position(|it| it.name == *sub) {
            Some(i) => i,
            None => {
                items.push(ConfItem {
                    name: sub.clone(),
                    key: None,
                    command: None,
                    cmd: None,
                    where_: cfg.default_where,
                    if_exists: cfg.default_if_exists,
                    if_missing: cfg.default_if_missing,
                });
                items.len() - 1
            }
        };
        for entry in &section.entries {
            let Some(value) = &entry.value else { continue };
            match entry.key.to_ascii_lowercase().as_str() {
                "key" => items[idx].key = Some(value.clone()),
                "command" => items[idx].command = Some(value.clone()),
                "cmd" => items[idx].cmd = Some(value.clone()),
                "where" => {
                    if let Some(w) = parse_where(value) {
                        items[idx].where_ = w;
                    }
                }
                "ifexists" => {
                    if let Some(v) = parse_if_exists(value) {
                        items[idx].if_exists = v;
                    }
                }
                "ifmissing" => {
                    if let Some(v) = parse_if_missing(value) {
                        items[idx].if_missing = v;
                    }
                }
                _ => {}
            }
        }
    }
    cfg.conf_items = items;

    cfg
}

// The effective config (repo file + `-c`/env overlay) when inside a repository.
// `read_repo_config` already layers command-line `-c` / `GIT_CONFIG_*` overrides
// on top of the repo config file, so a single read gives us every `trailer.*`
// key including overrides. Best-effort; returns `None` outside a repository
// (git's compiled-in defaults then apply, matching real interpret-trailers which
// runs fine outside a repo).
// ---------------------------------------------------------------------------
// The trailer model and core processing engine live in `sley_mail::trailers`.
// ---------------------------------------------------------------------------
