//! `git interpret-trailers` engine: trailer-block detection, parsing,
//! `--trailer` application (where/if-exists/if-missing policies, configured
//! command trailers), and rendering. A faithful port of git's `trailer.c`,
//! extracted verbatim from
//! `sley-cli/src/commands/interpret_trailers.rs`. Configuration loading from
//! `GitConfig` stays in the CLI; this module works on plain-data types only.

/// Where a freshly applied trailer is placed relative to existing ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Where {
    End,
    Start,
    After,
    Before,
}

/// What to do when a trailer with the same token already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfExists {
    AddIfDifferentNeighbor,
    AddIfDifferent,
    Add,
    Replace,
    DoNothing,
}

/// What to do when no trailer with the same token exists yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfMissing {
    Add,
    DoNothing,
}

/// A trailer queued by `--trailer`. `token`/`value` are already split on the
/// argument separator but not yet whitespace-normalised for output.
///
/// `where_`/`if_exists`/`if_missing` are the *resolved* placement/policy (config
/// item, then command-line override). `command`/`cmd` carry the configured shell
/// command (if any) inherited from the matched config item, run lazily when the
/// trailer is applied (git's `apply_item_command`).
#[derive(Debug, Clone)]
pub struct ArgTrailer {
    pub token: String,
    pub value: String,
    pub where_: Where,
    pub if_exists: IfExists,
    pub if_missing: IfMissing,
    /// `trailer.<token>.command`: shell command with `$ARG` substituted.
    pub command: Option<String>,
    /// `trailer.<token>.cmd`: shell command run with the argument appended.
    pub cmd: Option<String>,
}

/// Fully parsed command-line options.
#[derive(Debug)]
pub struct TrailerOptions {
    pub trim_empty: bool,
    pub only_trailers: bool,
    pub only_input: bool,
    pub unfold: bool,
    pub no_divider: bool,
    /// Default placement/policy for trailers that don't override them.
    pub default_where: Where,
    pub default_if_exists: IfExists,
    pub default_if_missing: IfMissing,
    /// Output separator character (first of `trailer.separators`, default ':').
    pub out_separator: char,
    /// Set of characters that separate a token from its value when parsing.
    pub separators: Vec<char>,
    /// Comment-line prefix (default '#').
    pub comment_prefix: String,
    /// Per-token configured trailer items (`trailer.<name>.*`), in config order.
    /// Used to resolve a `--trailer <token>` against an alias/key and to inherit
    /// its placement/policy/command, and to seed config-command arg items.
    pub conf_items: Vec<ConfItem>,
    pub trailers: Vec<ArgTrailer>,
}

/// A configured trailer item (`trailer.<name>.key/command/cmd/where/ifexists/
/// ifmissing`). git keeps one of these per distinct `<name>`; a `--trailer`
/// whose token case-insensitively prefix-matches `name` (or `key`) inherits this
/// item's settings and rewrites its output token to `key` when one is set.
#[derive(Debug, Clone)]
pub struct ConfItem {
    /// The config subsection name (`trailer.<name>.*`).
    pub name: String,
    /// `trailer.<name>.key`: the canonical output token (may carry its own
    /// trailing separator, e.g. `Bug #`).
    pub key: Option<String>,
    pub command: Option<String>,
    pub cmd: Option<String>,
    /// Placement/policy, defaulting to the global defaults when unset on the item.
    pub where_: Where,
    pub if_exists: IfExists,
    pub if_missing: IfMissing,
}

/// git's `token_len_without_separator`: the length of the token up to (but not
/// including) a trailing separator character or trailing whitespace. Used so a
/// `--trailer Bug` matches a configured `trailer.bug.key = "Bug #"` whose own
/// trailing `#`/space are not part of the comparable token.
pub fn token_len_without_separator(token: &str, separators: &[char]) -> usize {
    let bytes = token.as_bytes();
    let mut len = bytes.len();
    while len > 0 {
        let c = bytes[len - 1] as char;
        if c.is_ascii_whitespace() || separators.contains(&c) {
            len -= 1;
        } else {
            break;
        }
    }
    len
}

/// git's `token_matches_item`: case-insensitive prefix comparison of the first
/// `tok_len` bytes of `tok` against the item's `name`, then (if set) its `key`.
pub fn token_matches_item(tok: &str, item: &ConfItem, tok_len: usize) -> bool {
    let tok_bytes = tok.as_bytes();
    let n = tok_len.min(tok_bytes.len());
    let prefix = &tok_bytes[..n];
    if prefix_eq_ignore_case(prefix, item.name.as_bytes()) {
        return true;
    }
    match &item.key {
        Some(key) => prefix_eq_ignore_case(prefix, key.as_bytes()),
        None => false,
    }
}

/// `strncasecmp(prefix, full, prefix.len()) == 0`: compare `prefix` against the
/// leading bytes of `full` case-insensitively. (git compares only `tok_len`
/// bytes, so a short token prefix-matches a longer configured name.)
pub fn prefix_eq_ignore_case(prefix: &[u8], full: &[u8]) -> bool {
    if prefix.len() > full.len() {
        return false;
    }
    prefix.eq_ignore_ascii_case(&full[..prefix.len()])
}

/// Split a `--trailer` argument into token/value using git's `find_separator`
/// with the separator set augmented by `=` (git always accepts `=` for
/// command-line trailers), then resolve it against the configured trailer items.
///
/// This mirrors git's `parse_trailer` + `add_arg_item`:
///   * The token before the separator and the value after it are both
///     whitespace-trimmed; with no valid separator the whole argument is the
///     token and the value is empty (so `Naïve=café`, whose token byte `ï` is not
///     a valid token character, keeps the literal `Naïve=café` as its token).
///   * The trimmed token is matched (case-insensitively, over the token's length
///     ignoring its own trailing separator) against each config item's `name` and
///     `key`. The *first* match supplies the conf (placement/policy/command) and,
///     when it has a `key`, rewrites the output token to that key.
///   * A later command-line `--where`/`--if-exists`/`--if-missing` overrides the
///     conf's placement/policy for this and subsequent trailers (`new_trailer_item`
///     in git); `None` means "no override — use the conf / global default".
#[allow(clippy::too_many_arguments)]
pub fn parse_trailer_arg(
    raw: &str,
    separators: &[char],
    conf_items: &[ConfItem],
    default_where: Where,
    default_if_exists: IfExists,
    default_if_missing: IfMissing,
    ov_where: Option<Where>,
    ov_if_exists: Option<IfExists>,
    ov_if_missing: Option<IfMissing>,
) -> ArgTrailer {
    // `=` plus the configured separators (deduplicated order does not matter:
    // find_separator returns the first matching byte).
    let mut arg_separators: Vec<char> = vec!['='];
    for &sep in separators {
        if sep != '=' {
            arg_separators.push(sep);
        }
    }
    let (raw_token, value) = match find_separator(raw, &arg_separators) {
        Some(i) => {
            let token = &raw[..i];
            // The separator is a single ASCII byte for the '='/':'-class chars
            // find_separator can match.
            let rest = &raw[i + 1..];
            (token, rest)
        }
        None => (raw, ""),
    };
    let token = raw_token.trim().to_string();

    // Resolve against the configured items. `token_len_without_separator` is the
    // token length up to (but not including) any trailing separator the token
    // itself carries (e.g. matching `Bug` against a `Bug #` token).
    let tok_len = token_len_without_separator(&token, separators);
    let mut out_token = token.clone();
    let mut where_ = default_where;
    let mut if_exists = default_if_exists;
    let mut if_missing = default_if_missing;
    let mut command = None;
    let mut cmd = None;
    for item in conf_items {
        if token_matches_item(&token, item, tok_len) {
            where_ = item.where_;
            if_exists = item.if_exists;
            if_missing = item.if_missing;
            command = item.command.clone();
            cmd = item.cmd.clone();
            if let Some(key) = &item.key {
                out_token = key.clone();
            }
            break;
        }
    }

    // Command-line override (set by a preceding --where/--if-exists/--if-missing)
    // wins over the conf, exactly like git's `new_trailer_item` fixup.
    if let Some(w) = ov_where {
        where_ = w;
    }
    if let Some(e) = ov_if_exists {
        if_exists = e;
    }
    if let Some(m) = ov_if_missing {
        if_missing = m;
    }

    ArgTrailer {
        token: out_token,
        value: value.trim().to_string(),
        where_,
        if_exists,
        if_missing,
        command,
        cmd,
    }
}

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
pub struct Trailer {
    /// `Some(token)` for a real trailer; `None` for a preserved raw line.
    pub token: Option<String>,
    /// Token value (possibly multi-line, embedded `\n`) or the verbatim raw line.
    pub value: String,
    /// The configured output separator at parse time, used when re-rendering.
    pub separator: char,
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
pub fn process_message(raw_input: &str, opts: &TrailerOptions) -> String {
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

    // Apply queued args (unless --only-input). git builds the arg list as the
    // config-command trailers (one per configured item with a `command`) spliced
    // *before* the command-line `--trailer` args, then processes them in order.
    if !opts.only_input {
        let mut args: Vec<ArgTrailer> = Vec::new();
        for item in &opts.conf_items {
            if item.command.is_some() {
                args.push(ArgTrailer {
                    // git uses token_from_item(item, NULL): the key if set, else
                    // the config name.
                    token: item.key.clone().unwrap_or_else(|| item.name.clone()),
                    value: String::new(),
                    where_: item.where_,
                    if_exists: item.if_exists,
                    if_missing: item.if_missing,
                    command: item.command.clone(),
                    cmd: item.cmd.clone(),
                });
            }
        }
        args.extend(opts.trailers.iter().cloned());
        for arg in &args {
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
pub fn next_line(buf: &str, pos: usize) -> usize {
    match buf.as_bytes()[pos..].iter().position(|&b| b == b'\n') {
        Some(rel) => pos + rel + 1,
        None => buf.len(),
    }
}

/// git's `last_line(buf, len)`: byte offset of the start of the last line within
/// `buf[..len]`, or `None` when the region is empty.
pub fn last_line(buf: &str, len: usize) -> Option<usize> {
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
pub fn is_blank_line_at(buf: &str, pos: usize) -> bool {
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
pub fn is_empty_line_at(buf: &str, pos: usize) -> bool {
    matches!(buf.as_bytes().get(pos), None | Some(b'\n'))
}

/// True when the last line of `buf` (the whole slice) is a blank line; used for
/// the body/trailer separator decision (`ends_with_blank_line`).
pub fn ends_with_blank_line(buf: &str) -> bool {
    match last_line(buf, buf.len()) {
        Some(ll) => is_blank_line_at(buf, ll),
        None => false,
    }
}

/// Does the line beginning at `pos` start with the comment prefix?
pub fn is_comment_line_at(buf: &str, pos: usize, comment_prefix: &str) -> bool {
    !comment_prefix.is_empty() && buf[pos..].starts_with(comment_prefix)
}

// ---------------------------------------------------------------------------
// Log-region boundaries
// ---------------------------------------------------------------------------

/// git's `find_end_of_log_message`: the editable log ends at the `---` divider
/// (unless `no_divider`), then trailing ignorable comment/blank bytes are
/// removed so they live with the patch tail rather than the trailer block.
pub fn find_end_of_log_message(input: &str, no_divider: bool, comment_prefix: &str) -> usize {
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
pub fn is_divider_at(input: &str, pos: usize) -> bool {
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
pub fn ignored_log_message_bytes(buf: &str, len: usize, comment_prefix: &str) -> usize {
    // `cutoff` = the position of the scissors ("cut") line, if any: everything
    // from there to `len` is below the cut and is ignored wholesale (git's
    // `wt_status_locate_end`). Absent a scissors line, `cutoff == len`.
    let cutoff = wt_status_locate_end(buf, len, comment_prefix);
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
    if boc != 0 { len - boc } else { len - cutoff }
}

/// git's `wt_status_locate_end`: find the "scissors" (cut) line within
/// `buf[..len]` and return the offset of its start (so everything from there on
/// is below the cut). The scissors line is `<comment> ------------------------ >8
/// ------------------------` on its own line. git matches the pattern
/// `\n<comment> <cut_line>\n` (the leading `\n` anchors it to a line boundary,
/// the trailing `\n` to a full line); when the buffer *starts* with the
/// comment+cut_line (no leading newline needed), the whole buffer is below the
/// cut (`len = 0`). Returns `len` when no scissors line is present.
pub fn wt_status_locate_end(buf: &str, len: usize, comment_prefix: &str) -> usize {
    const CUT_LINE: &str = "------------------------ >8 ------------------------\n";
    let region = &buf[..len];
    // pattern (without the leading '\n'): "<comment> <cut_line>"
    let head = format!("{comment_prefix} {CUT_LINE}");
    if region.starts_with(&head) {
        return 0;
    }
    // full pattern: "\n<comment> <cut_line>"
    let pattern = format!("\n{head}");
    match region.find(&pattern) {
        Some(p) => {
            // newlen = (p - s) + 1: just past the matched leading '\n', i.e. the
            // start of the scissors line itself.
            let newlen = p + 1;
            newlen.min(len)
        }
        None => len,
    }
}

// ---------------------------------------------------------------------------
// Trailer-block detection
// ---------------------------------------------------------------------------

/// git's `find_trailer_block_start`: scan backward over the final paragraph of
/// `buf[..len]` and return the byte offset where the trailer block begins. When
/// no trailer block is found (including the whole message being the title) this
/// returns `len`, i.e. an empty block at the end of the log region.
pub fn find_trailer_block_start(buf: &str, len: usize, opts: &TrailerOptions) -> usize {
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
                if (recognized_prefix && trailer_lines * 3 >= non_trailer_lines)
                    || (trailer_lines > 0 && non_trailer_lines == 0)
                {
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
                trailer_lines += 1;
                possible_continuation = 0;
                // git also marks `recognized_prefix` when this trailer line's
                // token matches a configured `trailer.<name>` item — a single
                // configured trailer in the paragraph then enables the 25% rule
                // (`trailer_lines * 3 >= non_trailer_lines`). The match uses the
                // separator position as the token length (git's
                // `token_matches_item(bol, item, separator_pos)`).
                if !recognized_prefix {
                    let line = line_at(buf, bol, len);
                    for item in &opts.conf_items {
                        if token_matches_item(line, item, sep) {
                            recognized_prefix = true;
                            break;
                        }
                    }
                }
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
pub fn line_at(buf: &str, pos: usize, len: usize) -> &str {
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
/// [`separator_index`]. Operates on bytes to match C's byte-wise `isalnum`.
pub fn find_separator(line: &str, separators: &[char]) -> Option<usize> {
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
pub fn separator_index(line: &str, separators: &[char]) -> Option<usize> {
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
/// attached to it.
pub struct RawTrailerLine {
    /// The first physical line (newline stripped).
    head: String,
    /// Continuation lines (leading whitespace preserved, newline stripped).
    continuation: Vec<String>,
}

/// Phase 1 (`trailer_block_get`): split the block into logical lines, attaching
/// a leading-whitespace line to the previous logical line only when that line
/// had a separator (git's `if (last && isspace(buf[0]))`).
pub fn split_block_lines(block: &str, separators: &[char]) -> Vec<RawTrailerLine> {
    let mut lines: Vec<RawTrailerLine> = Vec::new();
    let mut last_has_sep = false;
    for raw in block.split_inclusive('\n') {
        let text = raw.strip_suffix('\n').unwrap_or(raw);
        if text.starts_with([' ', '\t'])
            && last_has_sep
            && let Some(last) = lines.last_mut()
        {
            last.continuation.push(text.to_string());
            continue;
        }
        let has_separator = find_separator(text, separators).is_some_and(|p| p >= 1);
        lines.push(RawTrailerLine {
            head: text.to_string(),
            continuation: Vec::new(),
        });
        last_has_sep = has_separator;
    }
    lines
}

/// Phase 2 (`parse_trailers`): turn the merged logical lines into structured
/// [`Trailer`]s. Comment lines are dropped outright; a line with a valid
/// separator becomes a token item; any other line becomes a raw item — except
/// under `--only-trailers`, where non-token lines are dropped at parse time too.
pub fn parse_trailers(block: &str, opts: &TrailerOptions) -> Vec<Trailer> {
    let mut trailers: Vec<Trailer> = Vec::new();
    for line in split_block_lines(block, &opts.separators) {
        // git: `if (starts_with(trailer, comment_line_str)) continue;`
        if !opts.comment_prefix.is_empty() && line.head.starts_with(&opts.comment_prefix) {
            continue;
        }
        match separator_index(&line.head, &opts.separators) {
            Some(sep) => {
                // git's `parse_trailer` trims the token on both ends, then
                // rewrites it to the configured `key` when it matches a config
                // item — this happens even for *input* trailers (the conf=NULL
                // path only skips storing the conf pointer, not the token
                // rewrite), so a configured `trailer.<name>.key` reformats
                // matching input trailers (e.g. `Acked-by:` → `Acked-by= `).
                let raw_token = line.head[..sep].trim().to_string();
                let token = rewrite_token_via_config(&raw_token, opts);
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

/// Rewrite a parsed token to the configured `key` of the first config item it
/// matches (git's unconditional token rewrite in `parse_trailer`). When no item
/// matches, or the matched item has no `key`, the token is returned unchanged.
pub fn rewrite_token_via_config(token: &str, opts: &TrailerOptions) -> String {
    let tok_len = token_len_without_separator(token, &opts.separators);
    for item in &opts.conf_items {
        if token_matches_item(token, item, tok_len) {
            return item.key.clone().unwrap_or_else(|| token.to_string());
        }
    }
    token.to_string()
}

/// Faithful port of git's `unfold_value`: each newline plus the whitespace run
/// that follows it collapses to a single space; all other characters (including
/// spaces not preceded by a newline) are preserved; the result is trimmed.
/// Iterates over `char`s so multibyte UTF-8 values survive intact.
pub fn unfold_value(value: &str) -> String {
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
pub fn after_or_end(where_: Where) -> bool {
    matches!(where_, Where::After | Where::End)
}

/// git's `same_token`: case-insensitive comparison over the shorter of the two
/// tokens, so a prefix like `Ack` matches `Acked-by`. Raw items (no token) never
/// match (`if (!a->token) return 0`).
pub fn item_same_token(item: &Trailer, arg_token: &str) -> bool {
    match &item.token {
        Some(tok) => same_token(tok, arg_token),
        None => false,
    }
}

/// Token comparison over the shorter length (git's `same_token` core).
pub fn same_token(a: &str, b: &str) -> bool {
    let min_len = a.len().min(b.len());
    a.as_bytes()[..min_len].eq_ignore_ascii_case(&b.as_bytes()[..min_len])
}

/// git's `same_value`: case-insensitive value comparison.
pub fn same_value(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

pub fn same_trailer(item: &Trailer, arg: &ArgTrailer) -> bool {
    item_same_token(item, &arg.token) && same_value(&item.value, &arg.value)
}

/// Apply a single queued trailer to the list, honouring its where/if-exists/
/// if-missing policy. Mirrors `find_same_and_apply_arg` / `apply_arg_if_exists`
/// / `add_arg_to_input_list` from trailer.c, including the `on_tok` reference
/// (the list head/tail for start/end, the matched item for after/before) and the
/// neighbor-vs-whole-list distinction between the two `addIfDifferent` modes.
///
/// When the arg carries a `command`/`cmd`, git runs it (`apply_item_command`) at
/// the point of application — *before* the duplicate comparison — replacing the
/// arg value with the command output. The command argument is the arg's own value
/// if non-empty, else the matched input trailer's value (or empty for the missing
/// path). We resolve that value first, then proceed with the resolved trailer.
pub fn apply_arg(trailers: &mut Vec<Trailer>, arg: &ArgTrailer, out_sep: char) {
    let backwards = after_or_end(arg.where_);
    let middle = matches!(arg.where_, Where::After | Where::Before);
    let has_command = arg.command.is_some() || arg.cmd.is_some();

    // find_same_and_apply_arg: locate the first same-token item in the search
    // direction. `start_idx` is the tail (backwards) or head (forwards).
    if trailers.is_empty() {
        // No existing trailers at all => if-missing applies; insertion falls back
        // to start/end since there is no reference item.
        if matches!(arg.if_missing, IfMissing::Add) {
            let arg = resolve_command(arg, None, has_command);
            let new = Trailer::token_item(arg.token.clone(), arg.value, out_sep);
            insert_relative(trailers, new, None, backwards);
        }
        return;
    }
    let start_idx = if backwards { trailers.len() - 1 } else { 0 };
    let match_idx = find_same_token(trailers, &arg.token, backwards);

    let Some(in_idx) = match_idx else {
        // if-missing path: no same-token trailer exists.
        if matches!(arg.if_missing, IfMissing::Add) {
            let arg = resolve_command(arg, None, has_command);
            let new = Trailer::token_item(arg.token.clone(), arg.value, out_sep);
            // on_tok is start_tok (head/tail); insert relative to it.
            insert_relative(trailers, new, Some(start_idx), backwards);
        }
        return;
    };

    // on_tok index: the matched item for after/before, else start_tok.
    let on_idx = if middle { in_idx } else { start_idx };

    // Run the configured command (if any) against the matched input value, then
    // build the new item from the resolved value. DoNothing never runs it.
    let resolved;
    let arg: &ArgTrailer = if matches!(arg.if_exists, IfExists::DoNothing) {
        arg
    } else {
        let in_value = trailers[in_idx].value.clone();
        resolved = resolve_command(arg, Some(&in_value), has_command);
        &resolved
    };
    let new = Trailer::token_item(arg.token.clone(), arg.value.clone(), out_sep);

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

/// Resolve `arg`'s value through its configured `command`/`cmd` if it has one,
/// returning a copy of `arg` with the command output as its value. When the arg
/// has no command this is a cheap clone. Mirrors `apply_item_command`: the
/// command argument is `arg.value` if non-empty, else `in_value` (the matched
/// input trailer's value), else the empty string.
pub fn resolve_command(arg: &ArgTrailer, in_value: Option<&str>, has_command: bool) -> ArgTrailer {
    if !has_command {
        return arg.clone();
    }
    let cmd_arg: &str = if !arg.value.is_empty() {
        &arg.value
    } else {
        in_value.unwrap_or("")
    };
    let output = run_trailer_command(arg.command.as_deref(), arg.cmd.as_deref(), cmd_arg);
    let mut out = arg.clone();
    out.value = output;
    out
}

/// git's `apply_command`: run the configured shell command, returning its trimmed
/// stdout (or `""` on failure). For `cmd`, the command is run as `sh -c <cmd>`
/// with the argument appended as a positional parameter; for `command`, the
/// literal `$ARG` token in the command string is replaced with the argument and
/// the whole thing is run via the shell. git always uses `use_shell = 1`.
pub fn run_trailer_command(command: Option<&str>, cmd: Option<&str>, arg: &str) -> String {
    use std::process::Command;

    // git's `use_shell` wraps the program in `sh -c "<prog> \"$@\"" <prog> <args>`
    // when the program has no shell metacharacters it would otherwise just exec;
    // we always route through `sh -c` to match the documented behaviour (cmd gets
    // the arg as a positional; command has $ARG pre-substituted in the script).
    let output = if let Some(cmd) = cmd {
        // `<cmd> <arg>` — git pushes cmd then arg as argv, with use_shell. The
        // shell receives the joined string and the arg as $1 via the trailing
        // operands. Reproduce by running `sh -c '<cmd> "$@"' <cmd> <arg>`.
        Command::new("sh")
            .arg("-c")
            .arg(format!("{cmd} \"$@\""))
            .arg(cmd) // $0
            .arg(arg) // $1
            .output()
    } else if let Some(command) = command {
        let script = command.replace(TRAILER_ARG_STRING, arg);
        Command::new("sh").arg("-c").arg(script).output()
    } else {
        return String::new();
    };

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.trim().to_string()
        }
        _ => String::new(),
    }
}

/// git's `TRAILER_ARG_STRING`: the placeholder replaced by the argument in a
/// `trailer.<token>.command` string.
const TRAILER_ARG_STRING: &str = "$ARG";

/// Find the index of the first same-token trailer scanning in `backwards`
/// direction (from the tail when backwards, else from the head). Raw items never
/// match.
pub fn find_same_token(trailers: &[Trailer], token: &str, backwards: bool) -> Option<usize> {
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
pub fn check_if_different(
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
pub fn insert_relative(
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
pub fn push_trailer(out: &mut String, trailer: &Trailer, opts: &TrailerOptions) {
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
pub fn last_non_space_char(s: &str) -> Option<char> {
    s.chars().rev().find(|c| !c.is_whitespace())
}

/// True when `message` ends with a recognised trailer block containing at
/// least one trailer (git's `has_conforming_footer` via `trailer_iterator`).
/// Honouring `trailer.<name>.*` config is essential: a configured token can tip
/// the 25% rule so a mixed paragraph still counts as a trailer block.
pub fn message_has_conforming_trailer_block(message: &str, opts: &TrailerOptions) -> bool {
    let end = find_end_of_log_message(message, opts.no_divider, &opts.comment_prefix);
    let start = find_trailer_block_start(message, end, opts);
    if start >= end {
        return false;
    }
    !parse_trailers(&message[start..end], opts).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_opts() -> TrailerOptions {
        TrailerOptions {
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
        }
    }

    fn with_trailers(specs: &[(&str, &str)]) -> TrailerOptions {
        let mut opts = default_opts();
        for (token, value) in specs {
            opts.trailers.push(ArgTrailer {
                token: (*token).to_string(),
                value: (*value).to_string(),
                where_: Where::End,
                if_exists: IfExists::AddIfDifferentNeighbor,
                if_missing: IfMissing::Add,
                command: None,
                cmd: None,
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
        let no_conf: &[ConfItem] = &[];
        let parse = |raw: &str| {
            parse_trailer_arg(
                raw,
                &seps,
                no_conf,
                Where::End,
                IfExists::AddIfDifferentNeighbor,
                IfMissing::Add,
                None,
                None,
                None,
            )
        };
        let t = parse("key=a:b");
        assert_eq!(t.token, "key");
        assert_eq!(t.value, "a:b");

        let t2 = parse("key:a=b");
        assert_eq!(t2.token, "key");
        assert_eq!(t2.value, "a=b");

        let t3 = parse("keyonly");
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
