//! Whitespace-rule engine — a port of git's `ws.c` / `ws.h`.
//!
//! This is the single source of truth for git's `core.whitespace` rules. It
//! powers three consumers, all of which used to be stubbed out in sley:
//!
//! * `git diff --check` (and `diff-index`/`diff-tree --check`) — reports
//!   whitespace errors introduced by the `+` lines of a diff;
//! * `--ws-error-highlight` — paints whitespace errors in the patch body;
//! * `git apply --whitespace=warn|error|fix|strip|nowarn` — warns about,
//!   errors on, or fixes whitespace errors in the patch being applied.
//!
//! The surface mirrors git's exactly:
//!
//! * [`parse_whitespace_rule`] parses a `core.whitespace` / attribute value
//!   into a [`WsRule`] bitmask (the low 6 bits are the tab width);
//! * [`ws_check`] classifies a single line, returning the [`WsRule`] bits that
//!   fired (so the caller can format them with [`whitespace_error_string`]);
//! * [`ws_check_emit`] does the same but also writes the line with
//!   whitespace-error spans painted (the `--ws-error-highlight` path);
//! * [`ws_fix_copy`] copies a line while fixing its whitespace errors (the
//!   `apply --whitespace=fix` path);
//! * [`ws_blank_line`] / [`count_trailing_blank`] support the blank-at-EOF
//!   detection that lives outside the per-line check in git.

/// `core.whitespace` rule mask. The low 6 bits encode the tab width (git
/// supports up to 63); the high bits are the individual error rules.
///
/// This mirrors git's `unsigned ws_rule` exactly so the bit values match
/// git's `ws.h` constants byte-for-byte.
pub type WsRule = u32;

/// Trailing whitespace at end of line.
pub const WS_BLANK_AT_EOL: WsRule = 1 << 6;
/// A space appearing before a tab in the indentation.
pub const WS_SPACE_BEFORE_TAB: WsRule = 1 << 7;
/// Indentation that uses spaces instead of tabs (tab-width worth of leading
/// spaces beyond any tabs).
pub const WS_INDENT_WITH_NON_TAB: WsRule = 1 << 8;
/// A carriage return at end of line.
pub const WS_CR_AT_EOL: WsRule = 1 << 9;
/// A new blank line at the end of the file.
pub const WS_BLANK_AT_EOF: WsRule = 1 << 10;
/// A tab appearing in the indentation.
pub const WS_TAB_IN_INDENT: WsRule = 1 << 11;
/// An incomplete final line (no trailing newline).
pub const WS_INCOMPLETE_LINE: WsRule = 1 << 12;

/// `trailing-space` = blank-at-eol + blank-at-eof.
pub const WS_TRAILING_SPACE: WsRule = WS_BLANK_AT_EOL | WS_BLANK_AT_EOF;
/// git's default rule: trailing-space + space-before-tab + tab width 8.
pub const WS_DEFAULT_RULE: WsRule = WS_TRAILING_SPACE | WS_SPACE_BEFORE_TAB | 8;
/// The low 6 bits hold the tab width.
pub const WS_TAB_WIDTH_MASK: WsRule = (1 << 6) - 1;
/// Mask covering all whitespace rule bits (low 16).
pub const WS_RULE_MASK: WsRule = (1 << 16) - 1;

/// Extract the tab width from a rule (git's `ws_tab_width` macro).
#[inline]
pub fn ws_tab_width(rule: WsRule) -> usize {
    (rule & WS_TAB_WIDTH_MASK) as usize
}

struct RuleName {
    name: &'static str,
    bits: WsRule,
    /// Loosens (rather than tightens) error checking; excluded from the
    /// "all rules" set built for a `whitespace`-true attribute.
    loosens_error: bool,
    /// Excluded from the default rule set even when not loosening.
    exclude_default: bool,
}

const RULE_NAMES: &[RuleName] = &[
    RuleName {
        name: "trailing-space",
        bits: WS_TRAILING_SPACE,
        loosens_error: false,
        exclude_default: false,
    },
    RuleName {
        name: "space-before-tab",
        bits: WS_SPACE_BEFORE_TAB,
        loosens_error: false,
        exclude_default: false,
    },
    RuleName {
        name: "indent-with-non-tab",
        bits: WS_INDENT_WITH_NON_TAB,
        loosens_error: false,
        exclude_default: false,
    },
    RuleName {
        name: "cr-at-eol",
        bits: WS_CR_AT_EOL,
        loosens_error: true,
        exclude_default: false,
    },
    RuleName {
        name: "blank-at-eol",
        bits: WS_BLANK_AT_EOL,
        loosens_error: false,
        exclude_default: false,
    },
    RuleName {
        name: "blank-at-eof",
        bits: WS_BLANK_AT_EOF,
        loosens_error: false,
        exclude_default: false,
    },
    RuleName {
        name: "tab-in-indent",
        bits: WS_TAB_IN_INDENT,
        loosens_error: false,
        exclude_default: true,
    },
    RuleName {
        name: "incomplete-line",
        bits: WS_INCOMPLETE_LINE,
        loosens_error: false,
        exclude_default: false,
    },
];

/// Parse a `core.whitespace` / `whitespace` attribute value into a [`WsRule`].
///
/// Port of git's `parse_whitespace_rule`. Comma/whitespace-separated tokens,
/// each optionally `-`-negated; `tabwidth=N` sets the tab width (1..=63). A
/// rule combining both `tab-in-indent` and `indent-with-non-tab` is rejected
/// (git `die`s); we return [`None`] so the caller can surface the error.
pub fn parse_whitespace_rule(string: &str) -> Option<WsRule> {
    let bytes = string.as_bytes();
    let mut rule = WS_DEFAULT_RULE;
    let mut pos = 0usize;

    while pos < bytes.len() {
        // Skip leading separators (`, \t\n\r`).
        while pos < bytes.len() && matches!(bytes[pos], b',' | b' ' | b'\t' | b'\n' | b'\r') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        // Token runs to the next comma (or end).
        let token_start = pos;
        let token_end = bytes[token_start..]
            .iter()
            .position(|&b| b == b',')
            .map(|off| token_start + off)
            .unwrap_or(bytes.len());

        let mut name_start = token_start;
        let mut negated = false;
        if bytes[name_start] == b'-' {
            negated = true;
            name_start += 1;
        }
        let name = &bytes[name_start..token_end];
        if name.is_empty() {
            break;
        }

        // git uses strncmp with the token length: a token matches a rule whose
        // name *starts with* the token bytes. (e.g. `incomplete` matches
        // `incomplete-line`.)
        for entry in RULE_NAMES {
            if entry.name.as_bytes().starts_with(name) {
                if negated {
                    rule &= !entry.bits;
                } else {
                    rule |= entry.bits;
                }
                break;
            }
        }

        // `tabwidth=N`. git tests the token after the negation strip
        // (`string` points past any leading `-`), so match from `name_start`.
        if let Some(arg) = token_starts_with_tabwidth(&bytes[name_start..token_end]) {
            let digits: String = arg.iter().take_while(|b| b.is_ascii_digit()).map(|&b| b as char).collect();
            let tabwidth: u32 = digits.parse().unwrap_or(0);
            if tabwidth > 0 && tabwidth < 0o100 {
                rule &= !WS_TAB_WIDTH_MASK;
                rule |= tabwidth;
            }
            // Out-of-range tab widths are silently ignored here (git warns).
        }

        pos = token_end;
    }

    if rule & WS_TAB_IN_INDENT != 0 && rule & WS_INDENT_WITH_NON_TAB != 0 {
        return None;
    }
    Some(rule)
}

fn token_starts_with_tabwidth(token: &[u8]) -> Option<&[u8]> {
    const PREFIX: &[u8] = b"tabwidth=";
    token.strip_prefix(PREFIX)
}

/// The `whitespace` gitattribute state for a path, the way git's
/// `whitespace_rule` interprets it.
pub enum WsAttr<'a> {
    /// `path whitespace` — true: enforce all (non-loosening, non-excluded)
    /// rules at the config's tab width.
    True,
    /// `path -whitespace` — false: enforce nothing (just the config tab width).
    False,
    /// `path !whitespace` or unattributed — use the config rule as-is.
    Unset,
    /// `path whitespace=<value>` — parse the value as a rule string.
    Value(&'a str),
}

/// Resolve the effective whitespace rule for a path. Port of git's
/// `whitespace_rule`: `config_rule` is the `core.whitespace` value (or
/// [`WS_DEFAULT_RULE`]), and `attr` is the per-path `whitespace` attribute.
///
/// Returns [`None`] only when an explicit attribute *value* names a
/// conflicting rule (git would `die`).
pub fn resolve_whitespace_rule(config_rule: WsRule, attr: WsAttr<'_>) -> Option<WsRule> {
    match attr {
        WsAttr::True => {
            // All enforcing rules at the config tab width.
            let mut all = config_rule & WS_TAB_WIDTH_MASK;
            for entry in RULE_NAMES {
                if !entry.loosens_error && !entry.exclude_default {
                    all |= entry.bits;
                }
            }
            Some(all)
        }
        // `-whitespace`: enforce nothing but keep the config tab width.
        WsAttr::False => Some(config_rule & WS_TAB_WIDTH_MASK),
        // `!whitespace` / unattributed: the config rule as-is.
        WsAttr::Unset => Some(config_rule),
        WsAttr::Value(value) => parse_whitespace_rule(value),
    }
}

/// Format the set of fired rule bits into git's human-readable error string.
///
/// Port of `whitespace_error_string`. The order and the `trailing whitespace`
/// collapsing of `WS_TRAILING_SPACE` (blank-at-eol + blank-at-eof together)
/// match git exactly.
pub fn whitespace_error_string(ws: WsRule) -> String {
    let mut err = String::new();
    if (ws & WS_TRAILING_SPACE) == WS_TRAILING_SPACE {
        err.push_str("trailing whitespace");
    } else {
        if ws & WS_BLANK_AT_EOL != 0 {
            err.push_str("trailing whitespace");
        }
        if ws & WS_BLANK_AT_EOF != 0 {
            if !err.is_empty() {
                err.push_str(", ");
            }
            err.push_str("new blank line at EOF");
        }
    }
    if ws & WS_SPACE_BEFORE_TAB != 0 {
        if !err.is_empty() {
            err.push_str(", ");
        }
        err.push_str("space before tab in indent");
    }
    if ws & WS_INDENT_WITH_NON_TAB != 0 {
        if !err.is_empty() {
            err.push_str(", ");
        }
        err.push_str("indent with spaces");
    }
    if ws & WS_TAB_IN_INDENT != 0 {
        if !err.is_empty() {
            err.push_str(", ");
        }
        err.push_str("tab in indent");
    }
    if ws & WS_INCOMPLETE_LINE != 0 {
        if !err.is_empty() {
            err.push_str(", ");
        }
        err.push_str("no newline at the end of file");
    }
    err
}

/// ASCII `isspace`, matching git's C locale behaviour (space, `\t`, `\n`,
/// `\x0b`, `\x0c`, `\r`).
#[inline]
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The painted spans produced by [`ws_check_emit`] for `--ws-error-highlight`.
///
/// `set`/`reset`/`ws` are the color escapes for the normal line color, the
/// reset, and the whitespace-error highlight. When color is off they are all
/// empty and the output is just the original line bytes.
pub struct WsEmitColors<'a> {
    /// `color.diff.new` (or the relevant line color) — normal text.
    pub set: &'a str,
    /// The reset escape.
    pub reset: &'a str,
    /// `color.diff.whitespace` — the whitespace-error highlight.
    pub ws: &'a str,
}

/// Classify a single line's whitespace errors, returning the rule bits that
/// fired. Port of `ws_check` (`ws_check_emit_1` with no stream).
///
/// `line` is the raw line content *without* the diff `+` prefix, and may
/// include a trailing `\n`.
pub fn ws_check(line: &[u8], ws_rule: WsRule) -> WsRule {
    ws_check_emit_inner(line, ws_rule, None)
}

/// Like [`ws_check`] but also appends the line to `out` with whitespace-error
/// spans painted using `colors`. Port of `ws_check_emit`.
pub fn ws_check_emit(line: &[u8], ws_rule: WsRule, out: &mut Vec<u8>, colors: &WsEmitColors<'_>) -> WsRule {
    ws_check_emit_inner(line, ws_rule, Some((out, colors)))
}

fn ws_check_emit_inner(
    line: &[u8],
    ws_rule: WsRule,
    mut stream: Option<(&mut Vec<u8>, &WsEmitColors<'_>)>,
) -> WsRule {
    let mut result: WsRule = 0;
    let mut written = 0usize;
    let mut trailing_whitespace: isize = -1;
    let mut trailing_newline = false;
    let mut trailing_carriage_return = false;

    let mut len = line.len();

    // Logic is simpler if we temporarily ignore the trailing newline.
    if len > 0 && line[len - 1] == b'\n' {
        trailing_newline = true;
        len -= 1;
    }
    if (ws_rule & WS_CR_AT_EOL) != 0 && len > 0 && line[len - 1] == b'\r' {
        trailing_carriage_return = true;
        len -= 1;
    }

    // Check for trailing whitespace.
    if ws_rule & WS_BLANK_AT_EOL != 0 {
        let mut i = len as isize - 1;
        while i >= 0 {
            if is_space(line[i as usize]) {
                trailing_whitespace = i;
                result |= WS_BLANK_AT_EOL;
            } else {
                break;
            }
            i -= 1;
        }
    }

    if trailing_whitespace == -1 {
        trailing_whitespace = len as isize;
    }
    let trailing_whitespace = trailing_whitespace as usize;

    if !trailing_newline && (ws_rule & WS_INCOMPLETE_LINE) != 0 {
        result |= WS_INCOMPLETE_LINE;
    }

    // Check indentation.
    let mut i = 0usize;
    while i < trailing_whitespace {
        if line[i] == b' ' {
            i += 1;
            continue;
        }
        if line[i] != b'\t' {
            break;
        }
        if (ws_rule & WS_SPACE_BEFORE_TAB) != 0 && written < i {
            result |= WS_SPACE_BEFORE_TAB;
            if let Some((out, colors)) = stream.as_mut() {
                out.extend_from_slice(colors.ws.as_bytes());
                out.extend_from_slice(&line[written..i]);
                out.extend_from_slice(colors.reset.as_bytes());
                out.push(line[i]);
            }
        } else if (ws_rule & WS_TAB_IN_INDENT) != 0 {
            result |= WS_TAB_IN_INDENT;
            if let Some((out, colors)) = stream.as_mut() {
                out.extend_from_slice(&line[written..i]);
                out.extend_from_slice(colors.ws.as_bytes());
                out.push(line[i]);
                out.extend_from_slice(colors.reset.as_bytes());
            }
        } else if let Some((out, _)) = stream.as_mut() {
            out.extend_from_slice(&line[written..=i]);
        }
        written = i + 1;
        i += 1;
    }

    // Check for indent using non-tab.
    if (ws_rule & WS_INDENT_WITH_NON_TAB) != 0 && i - written >= ws_tab_width(ws_rule) {
        result |= WS_INDENT_WITH_NON_TAB;
        if let Some((out, colors)) = stream.as_mut() {
            out.extend_from_slice(colors.ws.as_bytes());
            out.extend_from_slice(&line[written..i]);
            out.extend_from_slice(colors.reset.as_bytes());
        }
        written = i;
    }

    if let Some((out, colors)) = stream.as_mut() {
        // Emit non-highlighted (middle) segment.
        if trailing_whitespace > written {
            out.extend_from_slice(colors.set.as_bytes());
            out.extend_from_slice(&line[written..trailing_whitespace]);
            out.extend_from_slice(colors.reset.as_bytes());
        }
        // Highlight errors in trailing whitespace.
        if trailing_whitespace != len {
            out.extend_from_slice(colors.ws.as_bytes());
            out.extend_from_slice(&line[trailing_whitespace..len]);
            out.extend_from_slice(colors.reset.as_bytes());
        }
        if trailing_carriage_return {
            out.push(b'\r');
        }
        if trailing_newline {
            out.push(b'\n');
        }
    }

    result
}

/// Is the line entirely blank (whitespace only)? Port of `ws_blank_line`.
pub fn ws_blank_line(line: &[u8]) -> bool {
    line.iter().all(|&b| is_space(b))
}

/// Count the trailing run of blank lines in a buffer. Port of
/// `count_trailing_blank` (diff.c) — used by the blank-at-EOF detection.
///
/// The final newline is skipped (it does not count as a blank line); an
/// incomplete final line is treated as content. Returns the number of blank
/// lines at the very end of the buffer.
pub fn count_trailing_blank(buf: &[u8]) -> usize {
    let size = buf.len();
    if size == 0 {
        return 0;
    }
    let mut cnt = 0usize;
    // `ptr` is an index pointing at the last byte considered.
    let mut ptr: isize = size as isize - 1;
    if buf[ptr as usize] == b'\n' {
        ptr -= 1; // skip the last LF
    }
    // else: incomplete final line — the byte at ptr is part of it.
    let base: isize = 0;
    while base < ptr {
        // Find the previous LF at or below ptr (but above base-1).
        let mut prev_eol = ptr;
        while base <= prev_eol {
            if buf[prev_eol as usize] == b'\n' {
                break;
            }
            prev_eol -= 1;
        }
        // The line is buf[prev_eol+1 ..= ptr].
        let start = (prev_eol + 1) as usize;
        let end = (ptr + 1) as usize;
        if !ws_blank_line(&buf[start..end]) {
            break;
        }
        cnt += 1;
        ptr = prev_eol - 1;
    }
    cnt
}

/// Count the lines in a buffer the way git's `count_lines` does: a final line
/// without a trailing newline still counts.
pub fn count_lines(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let nl = buf.iter().filter(|&&b| b == b'\n').count();
    if buf[buf.len() - 1] == b'\n' {
        nl
    } else {
        nl + 1
    }
}

/// Copy `src` onto the end of `dst` while fixing whitespace errors per
/// `ws_rule`. Port of `ws_fix_copy`. Returns whether anything was fixed (git's
/// `error_count` increment) so callers can count fixes.
///
/// `src` is the line content (typically ending in `\n`, unless it is the
/// incomplete last line).
pub fn ws_fix_copy(dst: &mut Vec<u8>, src: &[u8], ws_rule: WsRule) -> bool {
    let mut len = src.len();
    let mut src_off = 0usize;
    let mut add_nl_to_tail = false;
    let mut add_cr_to_tail = false;
    let mut fixed = false;
    let mut last_tab_in_indent: isize = -1;
    let mut last_space_in_indent: isize = -1;
    let mut need_fix_leading_space = false;

    // An incomplete line is fixed by remembering to add the trailing newline.
    if ws_rule & WS_INCOMPLETE_LINE != 0 && len > 0 && src[len - 1] != b'\n' {
        fixed = true;
        add_nl_to_tail = true;
    }

    // Strip trailing whitespace.
    if ws_rule & WS_BLANK_AT_EOL != 0 {
        if len > 0 && src[len - 1] == b'\n' {
            add_nl_to_tail = true;
            len -= 1;
            if len > 0 && src[len - 1] == b'\r' {
                add_cr_to_tail = ws_rule & WS_CR_AT_EOL != 0;
                len -= 1;
            }
        }
        if len > 0 && is_space(src[len - 1]) {
            while len > 0 && is_space(src[len - 1]) {
                len -= 1;
            }
            fixed = true;
        }
    }

    // Check leading whitespace (indent).
    {
        let mut i = 0usize;
        while i < len {
            let ch = src[i];
            if ch == b'\t' {
                last_tab_in_indent = i as isize;
                if (ws_rule & WS_SPACE_BEFORE_TAB) != 0 && last_space_in_indent >= 0 {
                    need_fix_leading_space = true;
                }
            } else if ch == b' ' {
                last_space_in_indent = i as isize;
                if (ws_rule & WS_INDENT_WITH_NON_TAB) != 0
                    && (i as isize - last_tab_in_indent) >= ws_tab_width(ws_rule) as isize
                {
                    need_fix_leading_space = true;
                }
            } else {
                break;
            }
            i += 1;
        }
    }

    if need_fix_leading_space {
        // Process indent ourselves.
        let mut consecutive_spaces = 0usize;
        let mut last = (last_tab_in_indent + 1) as usize;
        if ws_rule & WS_INDENT_WITH_NON_TAB != 0 {
            // Point `last` one past the indent.
            if last_tab_in_indent < last_space_in_indent {
                last = (last_space_in_indent + 1) as usize;
            } else {
                last = (last_tab_in_indent + 1) as usize;
            }
        }
        let tabw = ws_tab_width(ws_rule);
        for &ch in &src[src_off..src_off + last] {
            if ch != b' ' {
                consecutive_spaces = 0;
                dst.push(ch);
            } else {
                consecutive_spaces += 1;
                if tabw != 0 && consecutive_spaces == tabw {
                    dst.push(b'\t');
                    consecutive_spaces = 0;
                }
            }
        }
        while consecutive_spaces > 0 {
            dst.push(b' ');
            consecutive_spaces -= 1;
        }
        len -= last;
        src_off += last;
        fixed = true;
    } else if (ws_rule & WS_TAB_IN_INDENT) != 0 && last_tab_in_indent >= 0 {
        // Expand tabs into spaces.
        let start = dst.len();
        let last = (last_tab_in_indent + 1) as usize;
        let tabw = ws_tab_width(ws_rule).max(1);
        for &ch in &src[src_off..src_off + last] {
            if ch == b'\t' {
                loop {
                    dst.push(b' ');
                    if (dst.len() - start).is_multiple_of(tabw) {
                        break;
                    }
                }
            } else {
                dst.push(ch);
            }
        }
        len -= last;
        src_off += last;
        fixed = true;
    }

    dst.extend_from_slice(&src[src_off..src_off + len]);
    if add_cr_to_tail {
        dst.push(b'\r');
    }
    if add_nl_to_tail {
        dst.push(b'\n');
    }
    fixed
}

/// Fix the whitespace of a single line's *content* (no trailing newline — the
/// caller stores newlines separately). Returns the fixed bytes; the caller can
/// compare against the input to know whether anything changed.
///
/// This is [`ws_fix_copy`] applied to a newline-free line: the trailing-newline
/// bookkeeping is inert, so the result is just the indent-fixed, trailing-ws-
/// stripped content.
pub fn ws_fix_line_content(content: &[u8], ws_rule: WsRule) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len());
    ws_fix_copy(&mut out, content, ws_rule);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rule_constant() {
        // trailing-space (eol|eof) + space-before-tab + tabwidth 8.
        assert_eq!(WS_DEFAULT_RULE, (1 << 6) | (1 << 10) | (1 << 7) | 8);
        assert_eq!(ws_tab_width(WS_DEFAULT_RULE), 8);
    }

    #[test]
    fn parse_basic() {
        // -trailing,-space-before,-indent disables those.
        let r = parse_whitespace_rule("-trailing,-space-before,-indent").expect("valid whitespace rule");
        assert_eq!(r & WS_BLANK_AT_EOL, 0);
        assert_eq!(r & WS_SPACE_BEFORE_TAB, 0);
    }

    #[test]
    fn parse_tab_in_indent_and_tabwidth() {
        let r = parse_whitespace_rule("-trailing,-space,-indent,tab").expect("valid whitespace rule");
        assert_ne!(r & WS_TAB_IN_INDENT, 0);
        let r2 = parse_whitespace_rule("tab-in-indent,tabwidth=16").expect("valid whitespace rule");
        assert_eq!(ws_tab_width(r2), 16);
    }

    #[test]
    fn parse_conflicting_rule_rejected() {
        assert!(parse_whitespace_rule("tab-in-indent,indent-with-non-tab").is_none());
    }

    #[test]
    fn trailing_whitespace_detected() {
        let r = WS_DEFAULT_RULE;
        assert_ne!(ws_check(b"foo(); \n", r) & WS_BLANK_AT_EOL, 0);
        assert_eq!(ws_check(b"foo();\n", r) & WS_BLANK_AT_EOL, 0);
    }

    #[test]
    fn space_before_tab_detected() {
        let r = WS_DEFAULT_RULE;
        // " \tfoo();" -> space before tab.
        assert_ne!(ws_check(b" \tfoo();\n", r) & WS_SPACE_BEFORE_TAB, 0);
    }

    #[test]
    fn indent_with_non_tab() {
        let r = parse_whitespace_rule("indent-with-non-tab").expect("valid whitespace rule");
        // 8 leading spaces (tab width 8) -> indent with spaces.
        assert_ne!(ws_check(b"        eight\n", r) & WS_INDENT_WITH_NON_TAB, 0);
        // 7 leading spaces -> not enough.
        assert_eq!(ws_check(b"       seven\n", r) & WS_INDENT_WITH_NON_TAB, 0);
    }

    #[test]
    fn error_string_order() {
        assert_eq!(whitespace_error_string(WS_TRAILING_SPACE), "trailing whitespace");
        assert_eq!(whitespace_error_string(WS_BLANK_AT_EOF), "new blank line at EOF");
        assert_eq!(
            whitespace_error_string(WS_SPACE_BEFORE_TAB | WS_TAB_IN_INDENT),
            "space before tab in indent, tab in indent"
        );
    }

    #[test]
    fn fix_strips_trailing() {
        let mut out = Vec::new();
        let fixed = ws_fix_copy(&mut out, b"foo(); \n", WS_DEFAULT_RULE);
        assert!(fixed);
        assert_eq!(out, b"foo();\n");
    }

    #[test]
    fn fix_tab_in_indent_expands() {
        let mut out = Vec::new();
        let r = parse_whitespace_rule("-trailing,-space,-indent,tab").expect("valid whitespace rule");
        // A leading tab expands to 8 spaces.
        ws_fix_copy(&mut out, b"\tfoo();\n", r);
        assert_eq!(out, b"        foo();\n");
    }

    #[test]
    fn count_trailing_blank_basic() {
        assert_eq!(count_trailing_blank(b"a\nb\n"), 0);
        assert_eq!(count_trailing_blank(b"a\nb\n\n"), 1);
        assert_eq!(count_trailing_blank(b"a\n\n\n"), 2);
        assert_eq!(count_trailing_blank(b"a\n   \n"), 1);
    }

    #[test]
    fn ws_check_emit_paints_trailing() {
        let colors = WsEmitColors {
            set: "<S>",
            reset: "<R>",
            ws: "<W>",
        };
        let mut out = Vec::new();
        ws_check_emit(b"foo(); \n", WS_DEFAULT_RULE, &mut out, &colors);
        assert_eq!(out, b"<S>foo();<R><W> <R>\n".to_vec());
    }
}
