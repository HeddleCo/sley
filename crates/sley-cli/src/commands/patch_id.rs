//! `git patch-id` — compute a stable identifier for a patch read from stdin.
//!
//! `git patch-id` reads a diff (as produced by `git diff`, `git log -p`,
//! `git show`, or `git format-patch`) on standard input and prints, for each
//! patch found, a line of the form
//!
//! ```text
//! <patch-id> <commit-id>
//! ```
//!
//! The patch id is a hash over the *content* of the diff with line numbers and
//! (by default) whitespace removed, so two patches that make the same textual
//! change yield the same id even if they apply at different offsets or were
//! reformatted. The trailing `<commit-id>` is the object name of the commit the
//! patch came from when the input carries one (a `commit <oid>` line from
//! `git log`, or a `From <oid> …` line from `git format-patch`); otherwise it is
//! all zeros.
//!
//! This is a faithful port of git's `builtin/patch-id.c` (`get_one_patchid` /
//! `flush_one_hunk` / `generate_id_list`). The algorithm, verified byte-for-byte
//! against the system `git` for plain, multi-file, binary, rename, mode-change,
//! `format-patch`, and multi-commit (`log -p`) inputs, is:
//!
//!   * Input is scanned line by line. A leading `commit <oid>` or `From <oid> …`
//!     line ends the current patch and records `<oid>` as the *next* patch's
//!     commit id (git emits the recorded id alongside the patch that follows it).
//!   * Within a diff, the `index <a>..<b>` line and the `@@ … @@` hunk headers are
//!     **not** hashed (the hunk header is parsed only for its line counts so the
//!     end of a hunk can be detected). Every other line — the `diff --git` header,
//!     `--- `/`+++ ` headers, `old mode`/`new mode`, and the `+`/`-`/` ` content —
//!     **is** hashed after whitespace removal.
//!   * `remove_space` drops *all* ASCII whitespace bytes (space, tab, newline,
//!     vertical tab, form feed, carriage return); `--verbatim` keeps every byte.
//!   * **Unstable** (the default) feeds the whole patch into one running hash, so
//!     the id depends on the order files appear in the diff.
//!   * **Stable** finalizes the running hash at every hunk/file boundary and folds
//!     each hunk digest into the result with a byte-wise **addition with carry**
//!     (not XOR), making the id independent of file/hunk ordering.
//!   * `--verbatim` implies stable and additionally keeps whitespace.
//!
//! The hash function follows the repository's object format (SHA-1, or SHA-256 in
//! a SHA-256 repository); outside any repository git uses SHA-1, which this code
//! mirrors. The default algorithm can be selected with the `patchid.stable`
//! config (`true` ⇒ stable, `false` ⇒ unstable); command-line flags override it.
//!
//! Like git, this command does not require a repository and produces no output for
//! input that contains no recognizable patch.
//!
//! This module follows the same glob-import + private-helper structure as the
//! other self-contained command modules (`commands::stash`, `commands::branch`,
//! `commands::verify_commit`); the wildcard pulls in shared crate-root plumbing
//! such as `cli_git_dir`, `repository_object_format`, `read_repo_config`,
//! `global_config_value`, and `sley_config::parse_config_bool`.
use crate::*;
use sley::plumbing::sley_config;

/// Exact usage text git's `patch-id` prints for `-h` and on option errors. A raw
/// string is used so the four-space indentation on the option lines (and the
/// trailing blank line) is preserved byte-for-byte; an escaped string with `\`
/// line-continuations would strip that leading whitespace.
const PATCH_ID_USAGE: &str = r#"usage: git patch-id [--stable | --unstable | --verbatim]

    --unstable            use the unstable patch ID algorithm
    --stable              use the stable patch ID algorithm
    --verbatim            don't strip whitespace from the patch

"#;

/// Which of the three mutually exclusive mode flags was supplied on the command
/// line. Used both to drive behavior and to reproduce git's incompatible-option
/// diagnostics, which name the flags as they were spelled.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PatchIdMode {
    Stable,
    Unstable,
    Verbatim,
}

impl PatchIdMode {
    /// The flag spelling git uses in its `cannot be used together` message.
    fn flag(self) -> &'static str {
        match self {
            PatchIdMode::Stable => "--stable",
            PatchIdMode::Unstable => "--unstable",
            PatchIdMode::Verbatim => "--verbatim",
        }
    }
}

/// Entry point for `git patch-id`.
pub(crate) fn cmd_patch_id(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = match parse_patch_id_args(cli_session, args)? {
        PatchIdInvocation::Run(options) => options,
        PatchIdInvocation::Help => {
            // `-h` prints usage to stdout and exits 129, like git's parse-options.
            print!("{PATCH_ID_USAGE}");
            io::stdout().flush()?;
            return Err(GitError::Exit(129));
        }
    };

    // patch-id works outside a repository; the hash width then follows SHA-1, the
    // same default git uses when there is no object-format to consult.
    let format = patch_id_object_format(cli_session)?;

    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;

    let lines = split_keep_newlines(&input);
    let mut out = io::stdout().lock();
    let mut cursor = 0usize;
    // The commit id to print with the *next* patch, carried across patches the way
    // git's `generate_id_list` threads `oid` from one iteration to the next.
    let mut pending_commit: Option<Vec<u8>> = None;
    while cursor < lines.len() {
        let patch = get_one_patchid(&lines, &mut cursor, format, &options);
        // git only prints a line when the patch had content (`patchlen` > 0),
        // which suppresses output for a bare `From <oid>` preamble or trailing
        // junk while still letting that preamble seed the next patch's commit id.
        if patch.patchlen > 0 {
            let result = ObjectId::from_raw(format, &patch.result)?;
            let default_commit = vec![b'0'; format.hex_len()];
            let commit = pending_commit.as_deref().unwrap_or(&default_commit);
            out.write_all(result.to_hex().as_bytes())?;
            out.write_all(b" ")?;
            out.write_all(commit)?;
            out.write_all(b"\n")?;
        }
        pending_commit = patch.next_commit;
    }
    out.flush()?;
    Ok(())
}

/// Compute the patch-id of a single rendered diff (as produced by
/// `git diff` / `render_tree_to_tree_patch`), for rebase's `--cherry-mark`
/// duplicate detection. Returns `None` when the diff carries no patch content
/// (e.g. an empty commit), so the caller can treat such commits as non-matching.
///
/// Thin delegation to [`sley_mail::patch_id`] (the hash core moved there);
/// unstable mode, matching git's default `--cherry-mark` behaviour.
pub(crate) fn patch_id_for_diff(diff: &[u8], format: ObjectFormat) -> Option<Vec<u8>> {
    sley_mail::patch_id::patch_id_for_diff(diff, format)
}

pub(crate) fn stable_patch_id_for_diff(diff: &[u8], format: ObjectFormat) -> Option<Vec<u8>> {
    sley_mail::patch_id::stable_patch_id_for_diff(diff, format)
}

/// The outcome of argument parsing: a runnable invocation or a help request.
enum PatchIdInvocation {
    Run(PatchIdOptions),
    Help,
}

/// Parse `patch-id` arguments, reproducing git's parse-options grammar: the three
/// mode flags are mutually exclusive, unambiguous prefixes are accepted, repeated
/// identical flags are fine, `--` ends option parsing, and any leftover operands
/// are ignored. Unknown options/switches and incompatible combinations exit 129.
fn parse_patch_id_args(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<PatchIdInvocation> {
    // The mode flag seen first on the command line, retained so a later,
    // *different* mode flag can be reported against it in declaration order.
    let mut chosen: Option<PatchIdMode> = None;
    let mut options_done = false;
    for arg in args {
        if options_done {
            // Operands are accepted and ignored, exactly like git.
            continue;
        }
        let arg = arg.as_str();
        if arg == "--" {
            options_done = true;
            continue;
        }
        if arg == "-h" || arg == "--help" {
            // git's `-h` and (here) `--help` both surface usage; real git execs
            // the man page for `--help`, which a hermetic context cannot do, so we
            // treat it as `-h`. The differential test only exercises `-h`.
            return Ok(PatchIdInvocation::Help);
        }
        if let Some(mode) = match_patch_id_mode(arg) {
            set_patch_id_mode(&mut chosen, mode)?;
            continue;
        }
        if let Some(name) = arg.strip_prefix("--") {
            return patch_id_unknown_option_error(name);
        }
        if arg.starts_with('-') && arg.len() > 1 {
            // git reports the first unrecognized short character as a "switch".
            let switch = arg.chars().nth(1).unwrap_or('-');
            return patch_id_unknown_switch_error(switch);
        }
        // A bare operand (not starting with `-`): ignored, like git.
    }

    // Resolve the effective behavior: an explicit flag wins; otherwise consult
    // `patchid.stable` (default unstable when unset).
    let options = match chosen {
        Some(PatchIdMode::Stable) => PatchIdOptions {
            stable: true,
            verbatim: false,
        },
        Some(PatchIdMode::Unstable) => PatchIdOptions {
            stable: false,
            verbatim: false,
        },
        Some(PatchIdMode::Verbatim) => PatchIdOptions {
            stable: true,
            verbatim: true,
        },
        None => patch_id_config_defaults(cli_session)?,
    };
    Ok(PatchIdInvocation::Run(options))
}

/// Match an argument against the three long options, accepting any unambiguous
/// prefix (git's parse-options does prefix matching). `--stable`/`--unstable`
/// share no common prefix, and `--verbatim` is distinct, so a non-empty prefix is
/// never ambiguous here. Returns `None` for anything that is not such a prefix.
fn match_patch_id_mode(arg: &str) -> Option<PatchIdMode> {
    let name = arg.strip_prefix("--")?;
    if name.is_empty() {
        return None;
    }
    if "stable".starts_with(name) {
        return Some(PatchIdMode::Stable);
    }
    if "unstable".starts_with(name) {
        return Some(PatchIdMode::Unstable);
    }
    if "verbatim".starts_with(name) {
        return Some(PatchIdMode::Verbatim);
    }
    None
}

/// Record a mode flag, erroring with git's incompatible-options message when a
/// *different* mode was already selected. Selecting the same mode twice is a no-op,
/// matching git's tolerance of repeated identical flags.
fn set_patch_id_mode(chosen: &mut Option<PatchIdMode>, mode: PatchIdMode) -> Result<()> {
    match *chosen {
        Some(existing) if existing != mode => {
            // git names the just-seen flag first, then the earlier one.
            eprintln!(
                "error: options '{}' and '{}' cannot be used together",
                mode.flag(),
                existing.flag()
            );
            Err(GitError::Exit(129))
        }
        _ => {
            *chosen = Some(mode);
            Ok(())
        }
    }
}

fn patch_id_unknown_option_error(option: &str) -> Result<PatchIdInvocation> {
    eprintln!("error: unknown option `{option}'");
    eprint!("{PATCH_ID_USAGE}");
    io::stderr().flush()?;
    Err(GitError::Exit(129))
}

fn patch_id_unknown_switch_error(switch: char) -> Result<PatchIdInvocation> {
    eprintln!("error: unknown switch `{switch}'");
    eprint!("{PATCH_ID_USAGE}");
    io::stderr().flush()?;
    Err(GitError::Exit(129))
}

/// Resolve default behavior from `patchid.stable` and `patchid.verbatim`.
///
/// An unset value defaults to unstable/non-verbatim, matching git. `verbatim`
/// implies `stable`. A value that is set but not a valid boolean is fatal with
/// git's exact "bad boolean config value" message.
fn patch_id_config_defaults(cli_session: &crate::session::CliSession) -> Result<PatchIdOptions> {
    let mut stable = false;
    let mut verbatim = false;
    if let Ok(git_dir) = cli_session.git_dir()
        && let Ok(config) = read_repo_config(&git_dir)
    {
        if let Some(value) = config.get_entry("patchid", None, "stable") {
            stable = interpret_patch_id_bool("patchid.stable", value)?;
        }
        if let Some(value) = config.get_entry("patchid", None, "verbatim") {
            verbatim = interpret_patch_id_bool("patchid.verbatim", value)?;
        }
    } else {
        if let Some(value) = global_config_value("patchid.stable")? {
            stable = interpret_patch_id_bool("patchid.stable", Some(&value))?;
        }
        if let Some(value) = global_config_value("patchid.verbatim")? {
            verbatim = interpret_patch_id_bool("patchid.verbatim", Some(&value))?;
        }
    }
    if verbatim {
        stable = true;
    }
    Ok(PatchIdOptions { stable, verbatim })
}

/// Parse a patch-id boolean config value, emitting git's fatal diagnostic for a
/// non-boolean string. A bare key (`patchid.stable` with no `=`) is true.
fn interpret_patch_id_bool(key: &str, value: Option<&str>) -> Result<bool> {
    let Some(value) = value else {
        return Ok(true);
    };
    match sley_config::parse_config_bool(value) {
        Some(flag) => Ok(flag),
        None => {
            eprintln!("fatal: bad boolean config value '{value}' for '{key}'");
            Err(GitError::Exit(128))
        }
    }
}

/// The hash algorithm patch-id should use: the repository's object format when in
/// a repository, else SHA-1 (git's choice with no repository to consult). A
/// repository whose config cannot be read falls back to SHA-1 as well.
fn patch_id_object_format(cli_session: &crate::session::CliSession) -> Result<ObjectFormat> {
    match cli_session.git_dir() {
        Ok(git_dir) => Ok(repository_object_format(&git_dir).unwrap_or(ObjectFormat::Sha1)),
        Err(_) => Ok(ObjectFormat::Sha1),
    }
}
