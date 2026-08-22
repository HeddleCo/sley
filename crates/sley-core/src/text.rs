//! Canonical text codecs shared across crates: shell single-quote rendering
//! (`sq_quote_buf` family, byte-parity ports of upstream git's quote.c for
//! 2.55), percent-encoding/decoding, and small formatting helpers built on
//! them. All crates must route quoting/percent work through this module so the
//! codecs stay single-homed and oracle-verified.

/// Punctuation git's `sq_quote_buf_pretty` leaves bare (quote.c `ok_punct`).
const PRETTY_SAFE_PUNCT: &[u8] = b"+,-./:=@_^";

const UPPER_HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

#[inline]
fn needs_bs_quote(byte: u8) -> bool {
    byte == b'\'' || byte == b'!'
}

/// Port of quote.c `sq_quote_buf`: always wrap in single quotes, escaping each
/// `'` or `!` as `'\''` / `'\!'`. The `!` escape guards against interactive
/// shells' history expansion, matching upstream 2.55 (`need_bs_quote`).
pub fn sq_quote_buf(out: &mut Vec<u8>, arg: &[u8]) {
    out.push(b'\'');
    for &byte in arg {
        if needs_bs_quote(byte) {
            out.extend_from_slice(b"'\\");
            out.push(byte);
            out.push(b'\'');
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
}

/// Port of quote.c `sq_quote_buf_pretty`: leave `arg` bare when it is non-empty
/// and every byte is ASCII alphanumeric or one of `+,-./:=@_^`; otherwise fall
/// back to [`sq_quote_buf`] semantics. An empty argument renders as `''`.
pub fn sq_quote_buf_pretty(out: &mut Vec<u8>, arg: &[u8]) {
    if !arg.is_empty()
        && arg
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || PRETTY_SAFE_PUNCT.contains(&byte))
    {
        out.extend_from_slice(arg);
        return;
    }
    sq_quote_buf(out, arg);
}

/// Convenience wrapper around [`sq_quote_buf`] for UTF-8 arguments.
pub fn sq_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for ch in arg.chars() {
        if ch == '\'' || ch == '!' {
            out.push_str("'\\");
            out.push(ch);
            out.push('\'');
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Convenience wrapper around [`sq_quote_buf_pretty`] for UTF-8 arguments.
pub fn sq_quote_pretty(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(
            |byte| byte.is_ascii_alphanumeric() || PRETTY_SAFE_PUNCT.contains(&byte),
        )
    {
        return arg.to_string();
    }
    sq_quote(arg)
}

/// Port of quote.c `sq_quote_argv`: prefix each argument with a space and
/// render it through full [`sq_quote`] semantics.
pub fn sq_quote_argv(args: &[String]) -> String {
    let mut out = String::new();
    for arg in args {
        out.push(' ');
        out.push_str(&sq_quote(arg));
    }
    out
}

/// Space-prefixed argv rendering used by trace2 `start` lines
/// (`sq_quote_argv_pretty`): each argument goes through
/// [`sq_quote_pretty`], joined by single spaces.
pub fn sq_quote_argv_pretty(args: &[String]) -> String {
    let mut out = String::new();
    for arg in args {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&sq_quote_pretty(arg));
    }
    out
}

/// Append `byte` to `out` as two uppercase hex digits.
pub fn hex_byte(out: &mut String, byte: u8) {
    out.push(UPPER_HEX_DIGITS[(byte >> 4) as usize] as char);
    out.push(UPPER_HEX_DIGITS[(byte & 0x0F) as usize] as char);
}

/// Safe-set mode table for [`percent_encode`], preserving the per-site
/// accept/reject behavior of current call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PercentEncodeMode {
    /// Pass through ASCII alphanumerics plus `_ . ~ / : -`.
    Field,
    /// [`PercentEncodeMode::Field`] plus `=`.
    OptionalField,
}

impl PercentEncodeMode {
    #[inline]
    fn allows(self, byte: u8) -> bool {
        match self {
            Self::Field => {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'~' | b'/' | b':' | b'-')
            }
            Self::OptionalField => Self::Field.allows(byte) || byte == b'=',
        }
    }
}

/// Percent-encode `value` into `out`: safe bytes pass through verbatim, every
/// other byte becomes an uppercase `%XX` escape.
pub fn percent_encode(out: &mut String, value: &[u8], mode: PercentEncodeMode) {
    for &byte in value {
        if mode.allows(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            hex_byte(out, byte);
        }
    }
}

/// Failure modes of [`percent_decode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PercentDecodeError {
    /// A `%` without two following bytes.
    TruncatedEscape,
    /// A `%XX` escape whose digit is not a hexadecimal character.
    InvalidHexDigit(u8),
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Strictly percent-decode `value`: every `%` must introduce a `%XX` escape
/// with case-insensitive hex digits; all other bytes are copied verbatim.
/// Returns raw bytes — callers own the UTF-8 validation and error wording.
pub fn percent_decode(value: &[u8]) -> Result<Vec<u8>, PercentDecodeError> {
    let mut out = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'%' {
            out.push(value[index]);
            index += 1;
            continue;
        }
        let Some(&high_digit) = value.get(index + 1) else {
            return Err(PercentDecodeError::TruncatedEscape);
        };
        let Some(&low_digit) = value.get(index + 2) else {
            return Err(PercentDecodeError::TruncatedEscape);
        };
        let high = hex_value(high_digit).ok_or(PercentDecodeError::InvalidHexDigit(high_digit))?;
        let low = hex_value(low_digit).ok_or(PercentDecodeError::InvalidHexDigit(low_digit))?;
        out.push((high << 4) | low);
        index += 3;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_argument_renders_as_two_quotes() {
        assert_eq!(sq_quote(""), "''");
        assert_eq!(sq_quote_pretty(""), "''");
        let mut buf = Vec::new();
        sq_quote_buf(&mut buf, b"");
        assert_eq!(buf, b"''");
        sq_quote_buf_pretty(&mut buf, b"");
        assert_eq!(buf, b"''''");
    }

    #[test]
    fn sq_quote_always_wraps_and_escapes_quote_and_bang() {
        assert_eq!(sq_quote("plain"), "'plain'");
        assert_eq!(sq_quote("v!1"), "'v'\\!'1'");
        assert_eq!(sq_quote("it's"), "'it'\\''s'");
        assert_eq!(sq_quote("a'b!c"), "'a'\\''b'\\!'c'");
        assert_eq!(sq_quote("back\\slash"), "'back\\slash'");
        // Non-ASCII passes through untouched inside the quotes.
        assert_eq!(sq_quote("café"), "'café'");
    }

    #[test]
    fn pretty_leaves_safe_sets_bare() {
        assert_eq!(sq_quote_pretty("plain"), "plain");
        assert_eq!(sq_quote_pretty("aBc123"), "aBc123");
        for punct in "+,-./:=@_^".chars() {
            let token = punct.to_string();
            assert_eq!(sq_quote_pretty(&token), token, "punct {punct} should stay bare");
        }
    }

    #[test]
    fn pretty_falls_back_to_full_quoting_for_unsafe_bytes() {
        assert_eq!(sq_quote_pretty("a b"), "'a b'");
        assert_eq!(sq_quote_pretty("v!1"), "'v'\\!'1'");
        assert_eq!(sq_quote_pretty("it's"), "'it'\\''s'");
        assert_eq!(sq_quote_pretty("tab\there"), "'tab\there'");
        // One unsafe byte anywhere forces whole-arg quoting.
        assert_eq!(sq_quote_pretty("safe!bang"), "'safe'\\!'bang'");
        // Non-ASCII bytes are not in the safe set.
        assert_eq!(sq_quote_pretty("café"), "'café'");
    }

    #[test]
    fn buf_and_str_variants_agree() {
        let to_string = |bytes: Vec<u8>| String::from_utf8(bytes).ok();
        for arg in [
            "", "plain", "v!1", "it's", "a b", "+,-./:=@_^", "café", "back\\slash",
        ] {
            let mut bytes = Vec::new();
            sq_quote_buf_pretty(&mut bytes, arg.as_bytes());
            assert_eq!(
                to_string(bytes).as_deref(),
                Some(sq_quote_pretty(arg).as_str()),
                "pretty mismatch for {arg:?}"
            );
            let mut bytes = Vec::new();
            sq_quote_buf(&mut bytes, arg.as_bytes());
            assert_eq!(
                to_string(bytes).as_deref(),
                Some(sq_quote(arg).as_str()),
                "buf mismatch for {arg:?}"
            );
        }
    }

    #[test]
    fn argv_helpers_space_prefix_each_argument() {
        assert_eq!(sq_quote_argv(&["git".into(), "log --oneline".into()]), " 'git' 'log --oneline'");
        assert_eq!(
            sq_quote_argv_pretty(&["git".into(), "log".into(), "v!1".into(), "".into()]),
            "git log 'v'\\!'1' ''"
        );
        assert_eq!(sq_quote_argv(&[]), "");
        assert_eq!(sq_quote_argv_pretty(&[]), "");
    }

    #[cfg(unix)]
    #[test]
    fn quoted_words_survive_sh_eval_round_trip() {
        use std::process::Command;

        // sq-quoted words are consumed by a single shell parse (this is how
        // git splices them into shell command lines), so the quoted text is
        // passed as part of one `sh -c` program — no extra eval layer.
        for value in ["plain", "v!1", "it's", "a b", "back\\slash", "$HOME", "`id`"] {
            let quoted = sq_quote(value);
            let script = format!("printf %s {quoted}");
            let output = Command::new("sh")
                .arg("-c")
                .arg(&script)
                .output()
                .expect("spawn sh");
            assert!(output.status.success(), "sh failed for {value:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                value,
                "round-trip failed for {value:?}"
            );
        }
    }

    #[test]
    fn hex_byte_formats_uppercase_pairs() {
        let mut out = String::new();
        hex_byte(&mut out, 0x00);
        hex_byte(&mut out, 0x0A);
        hex_byte(&mut out, 0xF0);
        hex_byte(&mut out, 0xFF);
        assert_eq!(out, "000AF0FF");
    }

    #[test]
    fn percent_encode_field_mode_table() {
        let mut out = String::new();
        percent_encode(&mut out, b"aZ0_.~/:-x", PercentEncodeMode::Field);
        assert_eq!(out, "aZ0_.~/:-x");

        out.clear();
        percent_encode(&mut out, b" ", PercentEncodeMode::Field);
        assert_eq!(out, "%20");

        out.clear();
        percent_encode(&mut out, b"=", PercentEncodeMode::Field);
        assert_eq!(out, "%3D");

        out.clear();
        percent_encode(&mut out, b"=", PercentEncodeMode::OptionalField);
        assert_eq!(out, "=");

        out.clear();
        percent_encode(&mut out, b"\xff\x01", PercentEncodeMode::Field);
        assert_eq!(out, "%FF%01");

        out.clear();
        percent_encode(&mut out, b"a=b c/d", PercentEncodeMode::OptionalField);
        assert_eq!(out, "a=b%20c/d");
    }

    #[test]
    fn percent_decode_is_strict_and_case_insensitive() {
        assert_eq!(
            percent_decode(b"plain").as_deref(),
            Ok(b"plain".as_slice())
        );
        assert_eq!(percent_decode(b"%41%62").as_deref(), Ok(b"Ab".as_slice()));
        assert_eq!(percent_decode(b"a%20b").as_deref(), Ok(b"a b".as_slice()));
        assert_eq!(
            percent_decode(b"ab%"),
            Err(PercentDecodeError::TruncatedEscape)
        );
        assert_eq!(
            percent_decode(b"ab%A"),
            Err(PercentDecodeError::TruncatedEscape)
        );
        assert_eq!(
            percent_decode(b"ab%G1"),
            Err(PercentDecodeError::InvalidHexDigit(b'G'))
        );
        assert_eq!(
            percent_decode(b"ab%1G"),
            Err(PercentDecodeError::InvalidHexDigit(b'G'))
        );
        // Raw bytes round-trip through encode/decode.
        for value in [b"".as_slice(), b"hello world", b"\x00\xff%25"] {
            let mut encoded = String::new();
            percent_encode(&mut encoded, value, PercentEncodeMode::Field);
            assert_eq!(percent_decode(encoded.as_bytes()).as_deref(), Ok(value));
        }
    }

    #[cfg(unix)]
    #[test]
    fn trace2_start_line_matches_oracle_layout() {
        // Oracle probe (git 2.55):
        //   GIT_TRACE2=1 git log --oneline -1 'v!1'
        //   -> start git log --oneline -1 'v'\!'1'
        assert_eq!(
            sq_quote_argv_pretty(&[
                "git".to_string(),
                "log".to_string(),
                "--oneline".to_string(),
                "-1".to_string(),
                "v!1".to_string(),
            ]),
            "git log --oneline -1 'v'\\!'1'"
        );
        // Empty args render as '' on the start line:
        //   GIT_TRACE2=1 git log -1 "" -> start git log -1 ''
        assert_eq!(
            sq_quote_argv_pretty(&[
                "git".to_string(),
                "log".to_string(),
                "-1".to_string(),
                "".to_string(),
            ]),
            "git log -1 ''"
        );
    }
}
