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
//! such as `discover_git_dir`, `repository_object_format`, `read_repo_config`,
//! `global_config_value`, and `sley_config::parse_config_bool`.
use crate::*;

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

/// Resolved behavior after merging command-line flags with `patchid.stable`.
struct PatchIdOptions {
    /// Use the order-independent stable algorithm (true for `--stable` and
    /// `--verbatim`, and when `patchid.stable` is true with no overriding flag).
    stable: bool,
    /// Keep whitespace instead of stripping it (`--verbatim`). Implies `stable`.
    verbatim: bool,
}

/// Entry point for `git patch-id`.
pub(crate) fn cmd_patch_id(args: &[String]) -> Result<()> {
    let options = match parse_patch_id_args(args)? {
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
    let format = patch_id_object_format()?;

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
            let commit = match &pending_commit {
                Some(oid) => oid.clone(),
                None => vec![b'0'; format.hex_len()],
            };
            out.write_all(result.to_hex().as_bytes())?;
            out.write_all(b" ")?;
            out.write_all(&commit)?;
            out.write_all(b"\n")?;
        }
        pending_commit = patch.next_commit;
    }
    out.flush()?;
    Ok(())
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
fn parse_patch_id_args(args: &[String]) -> Result<PatchIdInvocation> {
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
        None => PatchIdOptions {
            stable: patch_id_config_stable()?,
            verbatim: false,
        },
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

/// Resolve the default stable/unstable choice from `patchid.stable`.
///
/// Command-line `-c`/`GIT_CONFIG_*` overrides are consulted first (via
/// `global_config_value`), then repository-local config when inside a repository.
/// An unset value defaults to unstable, matching git. A value that is set but not
/// a valid boolean is fatal with git's exact "bad boolean config value" message.
fn patch_id_config_stable() -> Result<bool> {
    if let Some(value) = global_config_value("patchid.stable")? {
        return interpret_patch_id_stable(&value);
    }
    if let Ok(git_dir) = discover_git_dir(env::current_dir()?)
        && let Ok(config) = read_repo_config(&git_dir)
        && let Some(value) = config.get("patchid", None, "stable")
    {
        return interpret_patch_id_stable(value);
    }
    Ok(false)
}

/// Parse a `patchid.stable` value, emitting git's fatal diagnostic for a
/// non-boolean string.
fn interpret_patch_id_stable(value: &str) -> Result<bool> {
    match sley_config::parse_config_bool(value) {
        Some(flag) => Ok(flag),
        None => {
            eprintln!("fatal: bad boolean config value '{value}' for 'patchid.stable'");
            Err(GitError::Exit(128))
        }
    }
}

/// The hash algorithm patch-id should use: the repository's object format when in
/// a repository, else SHA-1 (git's choice with no repository to consult). A
/// repository whose config cannot be read falls back to SHA-1 as well.
fn patch_id_object_format() -> Result<ObjectFormat> {
    match discover_git_dir(env::current_dir()?) {
        Ok(git_dir) => Ok(repository_object_format(&git_dir).unwrap_or(ObjectFormat::Sha1)),
        Err(_) => Ok(ObjectFormat::Sha1),
    }
}

/// The accumulated state of one patch parsed from the input stream.
struct OnePatchId {
    /// The raw digest bytes of this patch's id (length matches the object format).
    result: Vec<u8>,
    /// The commit id recorded for the *following* patch, if a `commit`/`From`
    /// boundary line was consumed while scanning this one.
    next_commit: Option<Vec<u8>>,
    /// Total number of (post-`remove_space`) bytes hashed; zero means "no patch
    /// content", which suppresses output for this entry.
    patchlen: usize,
}

/// A running hash over a hunk's worth of patch bytes, plus the byte-wise
/// add-with-carry accumulator used to fold hunk digests in stable mode.
struct PatchHash {
    format: ObjectFormat,
    /// Bytes fed since the last flush; hashed lazily on `flush`/`finish` so the
    /// implementation stays independent of any incremental hashing API.
    buffer: Vec<u8>,
    /// The running result digest, folded into on every stable flush.
    result: Vec<u8>,
}

impl PatchHash {
    fn new(format: ObjectFormat) -> Self {
        PatchHash {
            format,
            buffer: Vec::new(),
            result: vec![0u8; format.raw_len()],
        }
    }

    /// Append bytes to the current hunk's hash input.
    fn update(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Finalize the current hunk and fold its digest into the running result with
    /// a byte-wise addition with carry, then reset for the next hunk. This is
    /// git's `flush_one_hunk`; it runs unconditionally (even on an empty buffer)
    /// so the digest of empty input participates exactly as git's does.
    fn flush(&mut self) -> Result<()> {
        let digest = sley_core::digest_bytes(self.format, &self.buffer)?;
        let bytes = digest.as_bytes();
        let mut carry: u16 = 0;
        for (slot, &add) in self.result.iter_mut().zip(bytes.iter()) {
            carry += u16::from(*slot) + u16::from(add);
            *slot = (carry & 0xff) as u8;
            carry >>= 8;
        }
        self.buffer.clear();
        Ok(())
    }

    /// Produce the final patch-id digest. Stable mode folds the trailing hunk via
    /// `flush` and returns the accumulator; unstable mode ignores the accumulator
    /// and returns the single hash of everything fed so far.
    fn finish(mut self, stable: bool) -> Result<Vec<u8>> {
        if stable {
            self.flush()?;
            Ok(self.result)
        } else {
            Ok(sley_core::digest_bytes(self.format, &self.buffer)?
                .as_bytes()
                .to_vec())
        }
    }
}

/// Parse the next patch from `lines` starting at `*cursor`, advancing `*cursor`
/// past the consumed lines. A faithful port of git's `get_one_patchid`.
fn get_one_patchid(
    lines: &[&[u8]],
    cursor: &mut usize,
    format: ObjectFormat,
    options: &PatchIdOptions,
) -> OnePatchId {
    let mut hash = PatchHash::new(format);
    let mut patchlen: usize = 0;
    // `before`/`after` track remaining context+removed / context+added lines in the
    // current hunk, exactly like git: -1 means "between hunks / parsing a header",
    // 0/0 means "hunk consumed, expecting the next `@@` or `diff`".
    let mut before: i64 = -1;
    let mut after: i64 = -1;
    let mut diff_is_binary = false;
    // The pre-/post-image object names captured from an `index` line, hashed only
    // when a binary patch follows (git hashes these as the binary hunk's content).
    let mut pre_oid: Vec<u8> = Vec::new();
    let mut post_oid: Vec<u8> = Vec::new();
    let mut next_commit: Option<Vec<u8>> = None;

    while *cursor < lines.len() {
        let line = lines[*cursor];
        *cursor += 1;

        // A `commit <oid>` / `From <oid> …` boundary records the next commit id and
        // ends this patch. A `\ No newline at end of file` marker is skipped (and,
        // under `--verbatim`, hashed verbatim) without affecting hunk accounting.
        if let Some(rest) = strip_line_prefix(line, b"commit ") {
            if let Some(oid) = leading_object_id(rest, format) {
                next_commit = Some(oid);
                break;
            }
        } else if let Some(rest) = strip_line_prefix(line, b"From ") {
            if let Some(oid) = leading_object_id(rest, format) {
                next_commit = Some(oid);
                break;
            }
        } else if line.starts_with(b"\\ ") && line.len() > 12 {
            if options.verbatim {
                hash.update(line);
            }
            continue;
        }

        // Skip commit-message text and other preamble until the first `diff` line.
        if patchlen == 0 && !line.starts_with(b"diff ") {
            continue;
        }

        // Parsing a diff header (no hunk seen yet for this file).
        if before == -1 {
            if line.starts_with(b"GIT binary patch") || line.starts_with(b"Binary files") {
                diff_is_binary = true;
                before = 0;
                hash.update(&pre_oid);
                hash.update(&post_oid);
                if options.stable {
                    // A flush error is impossible for in-memory hashing; ignore.
                    let _ = hash.flush();
                }
                continue;
            } else if let Some(rest) = strip_line_prefix(line, b"index ") {
                capture_index_oids(rest, &mut pre_oid, &mut post_oid);
                continue;
            } else if line.starts_with(b"--- ") {
                before = 1;
                after = 1;
            } else if !line.first().is_some_and(u8::is_ascii_alphabetic) {
                // A non-alphabetic line where a header was expected ends the patch
                // (e.g. trailing notes); leave it for the caller's next scan.
                *cursor -= 1;
                break;
            }
        }

        if diff_is_binary {
            if line.starts_with(b"diff ") {
                diff_is_binary = false;
                before = -1;
            }
            continue;
        }

        // Between hunks: either a new `@@` header or the start of the next file.
        if before == 0 && after == 0 {
            if line.starts_with(b"@@ -") {
                // Parse the next hunk's line counts; the header itself is not hashed.
                let (b, a) = scan_hunk_header(line);
                before = b;
                after = a;
                continue;
            }
            if !line.starts_with(b"diff ") {
                // End of this patch; let the caller re-read this line.
                *cursor -= 1;
                break;
            }
            if options.stable {
                let _ = hash.flush();
            }
            before = -1;
            after = -1;
        }

        // Inside a hunk: account for the line against the remaining counts.
        match line.first() {
            Some(b'-') => before -= 1,
            Some(b'+') => after -= 1,
            Some(b' ') => {
                before -= 1;
                after -= 1;
            }
            _ => {}
        }

        // Hash the line (whitespace-stripped unless `--verbatim`).
        if options.verbatim {
            patchlen += line.len();
            hash.update(line);
        } else {
            let stripped = remove_space(line);
            patchlen += stripped.len();
            hash.update(&stripped);
        }
    }

    let result = hash
        .finish(options.stable)
        .unwrap_or_else(|_| vec![0u8; format.raw_len()]);
    OnePatchId {
        result,
        next_commit,
        patchlen,
    }
}

/// Split a byte buffer into lines, keeping each line's trailing `\n` (a final line
/// without a newline is kept as-is). git reads whole lines including the newline,
/// then strips whitespace, so retaining the newline matters under `--verbatim`.
fn split_keep_newlines(input: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, &byte) in input.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&input[start..=index]);
            start = index + 1;
        }
    }
    if start < input.len() {
        lines.push(&input[start..]);
    }
    lines
}

/// Strip an exact byte prefix, returning the remainder when it matches.
fn strip_line_prefix<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    line.strip_prefix(prefix)
}

/// Parse a leading object id (exactly `hex_len` hex digits) from `bytes`, ignoring
/// any trailing content. Mirrors git's `get_oid_hex`, which only requires the
/// leading hex run and tolerates the `… Mon Sep 17 …` tail of a `From` line.
fn leading_object_id(bytes: &[u8], format: ObjectFormat) -> Option<Vec<u8>> {
    let width = format.hex_len();
    if bytes.len() < width {
        return None;
    }
    let head = &bytes[..width];
    if head.iter().all(u8::is_ascii_hexdigit) {
        Some(head.to_ascii_lowercase())
    } else {
        None
    }
}

/// Capture the pre-/post-image abbreviated object names from the body of an
/// `index <a>..<b>[ <mode>]` line, matching git's parsing (split at `..`, then at
/// the following space or end of line). Used only to seed a binary hunk's hash.
fn capture_index_oids(rest: &[u8], pre_oid: &mut Vec<u8>, post_oid: &mut Vec<u8>) {
    let Some(dots) = find_subslice(rest, b"..") else {
        return;
    };
    let pre = &rest[..dots];
    let after_dots = &rest[dots + 2..];
    // Stop the post-image at the first space (the mode), trimming a trailing
    // newline when no mode is present.
    let post_end = after_dots
        .iter()
        .position(|&byte| byte == b' ')
        .unwrap_or_else(|| trimmed_len(after_dots));
    let post = &after_dots[..post_end];
    *pre_oid = pre.to_vec();
    *post_oid = post.to_vec();
}

/// Length of `bytes` excluding a single trailing `\n`, if present.
fn trimmed_len(bytes: &[u8]) -> usize {
    match bytes.last() {
        Some(b'\n') => bytes.len() - 1,
        _ => bytes.len(),
    }
}

/// Find the first occurrence of `needle` within `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse the `-<l>,<n> +<l>,<n>` counts from a `@@ … @@` hunk header, ignoring the
/// line numbers (git's `scan_hunk_header`). A missing `,<n>` defaults to 1.
fn scan_hunk_header(line: &[u8]) -> (i64, i64) {
    // line begins with "@@ -"; parse "<old>[,<oldcount>] +<new>[,<newcount>]".
    let body = &line[b"@@ -".len()..];
    let (_old_start, old_count, after_old) = scan_range(body);
    // After the old range, skip up to and including the "+".
    let plus = match find_subslice(after_old, b"+") {
        Some(index) => &after_old[index + 1..],
        None => after_old,
    };
    let (_new_start, new_count, _rest) = scan_range(plus);
    (old_count, new_count)
}

/// Parse a `<number>[,<number>]` range at the start of `bytes`, returning the
/// start, the count (default 1 when no `,<count>` is present), and the remaining
/// bytes after the range.
fn scan_range(bytes: &[u8]) -> (i64, i64, &[u8]) {
    let (start, rest) = scan_number(bytes);
    if let Some(after_comma) = rest.strip_prefix(b",") {
        let (count, rest) = scan_number(after_comma);
        (start, count, rest)
    } else {
        (start, 1, rest)
    }
}

/// Parse a leading run of ASCII digits as an `i64`, returning the value (0 when no
/// digits) and the bytes following the run. Saturates rather than overflowing.
fn scan_number(bytes: &[u8]) -> (i64, &[u8]) {
    let mut value: i64 = 0;
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(bytes[index] - b'0'));
        index += 1;
    }
    (value, &bytes[index..])
}

/// Remove every ASCII whitespace byte from a line (git's `remove_space`): space,
/// tab, newline, vertical tab, form feed, and carriage return.
fn remove_space(line: &[u8]) -> Vec<u8> {
    line.iter()
        .copied()
        .filter(|byte| !is_patch_id_space(*byte))
        .collect()
}

/// Whether a byte is ASCII whitespace for `remove_space` purposes. Matches C's
/// `isspace` for the ASCII range git operates on.
fn is_patch_id_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}
